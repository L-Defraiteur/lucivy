use std::borrow::BorrowMut;
use std::collections::HashSet;
use std::io::Write;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use super::segment_manager::SegmentManager;
use super::segment_updater_actor::*;
use crate::actor::{mailbox, ActorRef, Envelope, Message, Scheduler};
use crate::core::META_FILEPATH;
use crate::directory::{Directory, DirectoryClone, GarbageCollectionResult};
use crate::fastfield::AliveBitSet;
use crate::index::{Index, IndexMeta, IndexSettings, Segment, SegmentId, SegmentMeta};
use crate::indexer::delete_queue::DeleteCursor;
use crate::indexer::merger::IndexMerger;
use crate::indexer::stamper::Stamper;
use crate::indexer::{
    DefaultMergePolicy, MergeOperation, MergePolicy, SegmentEntry,
    SegmentSerializer,
};
use crate::actor::events::{EventBus, EventReceiver};
use crate::indexer::events::IndexEvent;
use crate::{Opstamp, LucivyError};

/// Capacité de la mailbox du SegmentUpdaterActor.
const SEGMENT_UPDATER_MAILBOX_CAPACITY: usize = 128;

/// Save the index meta file.
/// This operation is atomic:
/// Either
/// - it fails, in which case an error is returned, and the `meta.json` remains untouched,
/// - it success, and `meta.json` is written and flushed.
///
/// This method is not part of lucivy's public API
pub(crate) fn save_metas(metas: &IndexMeta, directory: &dyn Directory) -> crate::Result<()> {
    info!("save metas");
    let mut buffer = serde_json::to_vec_pretty(metas)?;
    // Just adding a new line at the end of the buffer.
    writeln!(&mut buffer)?;
    crate::fail_point!("save_metas", |msg| Err(crate::LucivyError::from(
        std::io::Error::new(
            std::io::ErrorKind::Other,
            msg.unwrap_or_else(|| "Undefined".to_string())
        )
    )));
    directory.sync_directory()?;
    directory.atomic_write(&META_FILEPATH, &buffer[..])?;
    debug!("Saved metas {:?}", serde_json::to_string_pretty(&metas));
    Ok(())
}

/// État partagé entre le SegmentUpdater (facade) et le SegmentUpdaterActor.
///
/// Les champs utilisent de l'interior mutability (RwLock, AtomicBool)
/// pour être accessibles depuis plusieurs threads.
pub(crate) struct SegmentUpdaterShared {
    pub(crate) active_index_meta: RwLock<Arc<IndexMeta>>,
    pub(crate) index: Index,
    pub(crate) segment_manager: SegmentManager,
    pub(crate) merge_policy: RwLock<Arc<dyn MergePolicy>>,
    pub(crate) killed: AtomicBool,
    pub(crate) stamper: Stamper,
    pub(crate) event_bus: Arc<EventBus<IndexEvent>>,
}

impl SegmentUpdaterShared {
    pub(crate) fn save_metas(
        &self,
        opstamp: Opstamp,
        commit_message: Option<String>,
    ) -> crate::Result<()> {
        if self.is_alive() {
            let index = &self.index;
            let directory = index.directory();
            let mut committed_segment_metas = self.segment_manager.committed_segment_metas();
            committed_segment_metas.sort_by_key(|segment_meta| -(segment_meta.max_doc() as i32));
            let index_meta = IndexMeta {
                index_settings: index.settings().clone(),
                segments: committed_segment_metas,
                schema: index.schema(),
                opstamp,
                payload: commit_message,
            };
            save_metas(&index_meta, directory.box_clone().borrow_mut())?;
            self.store_meta(&index_meta);
        }
        Ok(())
    }

    pub(crate) fn store_meta(&self, index_meta: &IndexMeta) {
        *self.active_index_meta.write().unwrap() = Arc::new(index_meta.clone());
    }

    pub(crate) fn load_meta(&self) -> Arc<IndexMeta> {
        self.active_index_meta.read().unwrap().clone()
    }

    pub fn is_alive(&self) -> bool {
        !self.killed.load(Ordering::Acquire)
    }

    pub fn get_merge_policy(&self) -> Arc<dyn MergePolicy> {
        self.merge_policy.read().unwrap().clone()
    }

    pub(crate) fn purge_deletes(&self, target_opstamp: Opstamp) -> crate::Result<Vec<SegmentEntry>> {
        let mut segment_entries = self.segment_manager.segment_entries();
        for segment_entry in &mut segment_entries {
            let segment = self.index.segment(segment_entry.meta().clone());
            crate::indexer::index_writer::advance_deletes(segment, segment_entry, target_opstamp)?;
        }
        Ok(segment_entries)
    }

    pub(crate) fn get_mergeable_segments(
        &self,
        segments_in_merge: &HashSet<SegmentId>,
    ) -> (Vec<SegmentMeta>, Vec<SegmentMeta>) {
        self.segment_manager.get_mergeable_segments(segments_in_merge)
    }

    fn list_files(&self) -> HashSet<PathBuf> {
        let mut files: HashSet<PathBuf> = self
            .index
            .list_all_segment_metas()
            .into_iter()
            .flat_map(|segment_meta| segment_meta.list_files())
            .collect();
        files.insert(META_FILEPATH.to_path_buf());
        // Per-field .sfx files are listed in the manifest (SegmentComponent::SuffixFst).
        // The manifest itself is tracked by list_files() via SegmentComponent::iterator().
        // We read it to discover which per-field .sfx files to preserve from GC.
        for segment_meta in self.index.list_all_segment_metas() {
            let segment = self.index.segment(segment_meta.clone());
            if let Ok(manifest_slice) = segment.open_read(crate::index::SegmentComponent::SuffixFst) {
                if let Ok(manifest_data) = manifest_slice.read_bytes() {
                    if manifest_data.len() >= 4 {
                        let num = u32::from_le_bytes([
                            manifest_data[0], manifest_data[1],
                            manifest_data[2], manifest_data[3],
                        ]) as usize;
                        for i in 0..num {
                            let off = 4 + i * 4;
                            if off + 4 > manifest_data.len() { break; }
                            let fid = u32::from_le_bytes([
                                manifest_data[off], manifest_data[off+1],
                                manifest_data[off+2], manifest_data[off+3],
                            ]);
                            let uuid = segment_meta.id().uuid_string();
                            files.insert(PathBuf::from(format!("{uuid}.{fid}.sfx")));
                            files.insert(PathBuf::from(format!("{uuid}.{fid}.sfxpost")));
                        }
                    }
                }
            }
        }
        files
    }
}

pub(crate) fn garbage_collect_files(
    shared: &Arc<SegmentUpdaterShared>,
) -> crate::Result<GarbageCollectionResult> {
    info!("Running garbage collection");
    let shared_clone = shared.clone();
    let mut index = shared.index.clone();
    index
        .directory_mut()
        .garbage_collect(move || shared_clone.list_files())
}

/// Merges a list of segments the list of segment givens in the `segment_entries`.
/// This function happens in the calling thread and is computationally expensive.
#[allow(dead_code)]
pub(crate) fn merge(
    index: &Index,
    mut segment_entries: Vec<SegmentEntry>,
    target_opstamp: Opstamp,
) -> crate::Result<Option<SegmentEntry>> {
    let num_docs = segment_entries
        .iter()
        .map(|segment| segment.meta().num_docs() as u64)
        .sum::<u64>();
    if num_docs == 0 {
        return Ok(None);
    }

    let merged_segment = index.new_segment();

    for segment_entry in &mut segment_entries {
        let segment = index.segment(segment_entry.meta().clone());
        crate::indexer::index_writer::advance_deletes(segment, segment_entry, target_opstamp)?;
    }

    let delete_cursor = segment_entries[0].delete_cursor().clone();

    let segments: Vec<Segment> = segment_entries
        .iter()
        .map(|segment_entry| index.segment(segment_entry.meta().clone()))
        .collect();

    let merger: IndexMerger = IndexMerger::open(index.schema(), &segments[..])?;

    let segment_serializer = SegmentSerializer::for_segment(merged_segment.clone())?;

    let num_docs = merger.write(segment_serializer)?;

    let merged_segment_id = merged_segment.id();

    let segment_meta = index.new_segment_meta(merged_segment_id, num_docs);
    Ok(Some(SegmentEntry::new(segment_meta, delete_cursor, None)))
}

// ---------------------------------------------------------------------------
// SegmentUpdater — facade publique
// ---------------------------------------------------------------------------

/// Facade clonable pour interagir avec le SegmentUpdaterActor.
///
/// Remplace l'ancien `SegmentUpdater(Arc<InnerSegmentUpdater>)`.
/// Les opérations séquentielles (add segment, commit, merge, GC) passent
/// par l'acteur via des messages. Les opérations simples (is_alive, kill,
/// get/set_merge_policy) accèdent directement à l'état partagé.
#[derive(Clone)]
pub(crate) struct SegmentUpdater {
    shared: Arc<SegmentUpdaterShared>,
    actor_ref: ActorRef<Envelope>,
}

impl Deref for SegmentUpdater {
    type Target = SegmentUpdaterShared;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

impl SegmentUpdater {
    pub fn create(
        index: Index,
        stamper: Stamper,
        delete_cursor: &DeleteCursor,
        scheduler: Arc<Scheduler>,
    ) -> crate::Result<SegmentUpdater> {
        let segments = index.searchable_segment_metas()?;
        let segment_manager = SegmentManager::from_segments(segments, delete_cursor);
        let index_meta = index.load_metas()?;

        let shared = Arc::new(SegmentUpdaterShared {
            active_index_meta: RwLock::new(Arc::new(index_meta)),
            index,
            segment_manager,
            merge_policy: RwLock::new(Arc::new(DefaultMergePolicy::default())),
            killed: AtomicBool::new(false),
            stamper,
            event_bus: Arc::new(EventBus::new()),
        });

        let (mbox, mut aref) = mailbox::<Envelope>(SEGMENT_UPDATER_MAILBOX_CAPACITY);
        let actor = create_segment_updater_actor(shared.clone());
        scheduler.spawn(actor, mbox, &mut aref, SEGMENT_UPDATER_MAILBOX_CAPACITY);

        Ok(SegmentUpdater {
            shared,
            actor_ref: aref,
        })
    }

    pub fn subscribe_index_events(&self) -> EventReceiver<IndexEvent> {
        self.shared.event_bus.subscribe()
    }

    pub fn set_merge_policy(&self, merge_policy: Box<dyn MergePolicy>) {
        let arc_merge_policy = Arc::from(merge_policy);
        *self.shared.merge_policy.write().unwrap() = arc_merge_policy;
    }

    pub fn schedule_add_segment(&self, segment_entry: SegmentEntry) -> crate::Result<()> {
        if !self.is_alive() {
            return Err(LucivyError::SystemError("Segment updater killed".to_string()));
        }
        // Fire-and-forget : on n'attend pas la reply.
        // Cette méthode est appelée depuis IndexerActor::handle_flush,
        // c'est-à-dire depuis un thread du scheduler. Attendre ici bloquerait
        // le thread et provoquerait un deadlock quand tous les threads sont
        // occupés (doc 08 — cause racine des tests bloqués).
        self.actor_ref
            .send(SuAddSegmentMsg.into_envelope_with_local(segment_entry))
            .map_err(|_| {
                LucivyError::SystemError("Segment updater actor died".to_string())
            })?;
        Ok(())
    }

    /// Orders `SegmentManager` to remove all segments
    pub(crate) fn remove_all_segments(&self) {
        self.shared.segment_manager.remove_all_segments();
    }

    pub fn kill(&mut self) {
        self.shared.killed.store(true, Ordering::Release);
        let _ = self.actor_ref.send(SuKillMsg.into_envelope());
    }

    pub(crate) fn schedule_commit(
        &self,
        opstamp: Opstamp,
        payload: Option<String>,
    ) -> crate::Result<Opstamp> {
        self.schedule_commit_with_rebuild(opstamp, payload, true)
    }

    pub(crate) fn schedule_commit_with_rebuild(
        &self,
        opstamp: Opstamp,
        payload: Option<String>,
        rebuild_sfx: bool,
    ) -> crate::Result<Opstamp> {
        if !self.is_alive() {
            return Err(LucivyError::SystemError("Segment updater killed".to_string()));
        }
        let (env, rx) = SuCommitMsg { opstamp, payload, rebuild_sfx }.into_request();
        self.actor_ref
            .send(env)
            .map_err(|_| {
                LucivyError::SystemError("Segment updater actor died".to_string())
            })?;
        // Use wait_cooperative to avoid deadlock: the shard actor handler
        // calls this from a scheduler thread. wait_blocking would block that
        // thread, preventing the segment_updater from being dispatched.
        let scheduler = crate::actor::scheduler::global_scheduler();
        match rx.wait_cooperative_named("schedule_commit", || scheduler.run_one_step()) {
            Ok(bytes) => {
                let reply = SuOpsReply::decode(&bytes)
                    .map_err(|e| LucivyError::SystemError(e))?;
                Ok(reply.opstamp)
            }
            Err(err_bytes) => Err(
                LucivyError::decode(&err_bytes)
                    .unwrap_or_else(|e| LucivyError::SystemError(format!("decode: {e}")))
            ),
        }
    }

    pub fn schedule_garbage_collect(&self) -> crate::Result<GarbageCollectionResult> {
        if !self.is_alive() {
            return Err(LucivyError::SystemError("Segment updater killed".to_string()));
        }
        let (env, rx) = SuGarbageCollectMsg.into_request();
        self.actor_ref
            .send(env)
            .map_err(|_| {
                LucivyError::SystemError("Segment updater actor died".to_string())
            })?;
        match rx.wait_cooperative_named("segment_updater_op", || crate::actor::scheduler::global_scheduler().run_one_step()) {
            Ok(_) => garbage_collect_files(&self.shared),
            Err(err_bytes) => Err(
                LucivyError::decode(&err_bytes)
                    .unwrap_or_else(|e| LucivyError::SystemError(format!("decode: {e}")))
            ),
        }
    }

    pub(crate) fn make_merge_operation(&self, segment_ids: &[SegmentId]) -> MergeOperation {
        let commit_opstamp = self.load_meta().opstamp;
        MergeOperation::new(commit_opstamp, segment_ids.to_vec())
    }

    pub fn start_merge(
        &self,
        merge_operation: MergeOperation,
    ) -> crate::Result<Option<SegmentMeta>> {
        assert!(
            !merge_operation.segment_ids().is_empty(),
            "Segment_ids cannot be empty."
        );

        if !self.is_alive() {
            return Err(LucivyError::SystemError("Segment updater killed".to_string()));
        }
        let (env, rx) = SuStartMergeMsg.into_request_with_local(merge_operation);
        self.actor_ref
            .send(env)
            .map_err(|_| {
                LucivyError::SystemError("Segment updater actor died".to_string())
            })?;
        match rx.wait_cooperative_named("segment_updater_op", || crate::actor::scheduler::global_scheduler().run_one_step()) {
            Ok(_) => Ok(None), // StartMerge reply doesn't carry SegmentMeta in envelope mode
            Err(err_bytes) => Err(
                LucivyError::decode(&err_bytes)
                    .unwrap_or_else(|e| LucivyError::SystemError(format!("decode: {e}")))
            ),
        }
    }

    pub fn wait_merging_thread(&self) -> crate::Result<()> {
        if !self.is_alive() {
            return Ok(());
        }
        let (env, rx) = SuDrainMergesMsg.into_request();
        self.actor_ref
            .send(env)
            .map_err(|_| {
                LucivyError::SystemError("Segment updater actor died".to_string())
            })?;
        let _ = rx.wait_cooperative_named("segment_updater_op", || crate::actor::scheduler::global_scheduler().run_one_step());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public merge functions (unchanged)
// ---------------------------------------------------------------------------

/// Advanced: Merges a list of segments from different indices in a new index.
///
/// Returns `LucivyError` if the indices list is empty or their
/// schemas don't match.
///
/// `output_directory`: is assumed to be empty.
///
/// # Warning
/// This function does NOT check or take the `IndexWriter` is running. It is not
/// meant to work if you have an `IndexWriter` running for the origin indices, or
/// the destination `Index`.
#[doc(hidden)]
pub fn merge_indices<T: Into<Box<dyn Directory>>>(
    indices: &[Index],
    output_directory: T,
) -> crate::Result<Index> {
    if indices.is_empty() {
        return Err(crate::LucivyError::InvalidArgument(
            "No indices given to merge".to_string(),
        ));
    }

    let target_settings = indices[0].settings().clone();

    if indices
        .iter()
        .skip(1)
        .any(|index| index.settings() != &target_settings)
    {
        return Err(crate::LucivyError::InvalidArgument(
            "Attempt to merge indices with different index_settings".to_string(),
        ));
    }

    let mut segments: Vec<Segment> = Vec::new();
    for index in indices {
        segments.extend(index.searchable_segments()?);
    }

    let non_filter = segments.iter().map(|_| None).collect::<Vec<_>>();
    merge_filtered_segments(&segments, target_settings, non_filter, output_directory)
}

/// Advanced: Merges a list of segments from different indices in a new index.
/// Additional you can provide a delete bitset for each segment to ignore doc_ids.
///
/// Returns `LucivyError` if the indices list is empty or their
/// schemas don't match.
///
/// `output_directory`: is assumed to be empty.
///
/// # Warning
/// This function does NOT check or take the `IndexWriter` is running. It is not
/// meant to work if you have an `IndexWriter` running for the origin indices, or
/// the destination `Index`.
#[doc(hidden)]
pub fn merge_filtered_segments<T: Into<Box<dyn Directory>>>(
    segments: &[Segment],
    target_settings: IndexSettings,
    filter_doc_ids: Vec<Option<AliveBitSet>>,
    output_directory: T,
) -> crate::Result<Index> {
    if segments.is_empty() {
        return Err(crate::LucivyError::InvalidArgument(
            "No segments given to merge".to_string(),
        ));
    }

    let target_schema = segments[0].schema();

    if segments
        .iter()
        .skip(1)
        .any(|index| index.schema() != target_schema)
    {
        return Err(crate::LucivyError::InvalidArgument(
            "Attempt to merge different schema indices".to_string(),
        ));
    }

    let mut merged_index = Index::create(
        output_directory,
        target_schema.clone(),
        target_settings.clone(),
    )?;
    let merged_segment = merged_index.new_segment();
    let merged_segment_id = merged_segment.id();
    let merger: IndexMerger =
        IndexMerger::open_with_custom_alive_set(merged_index.schema(), segments, filter_doc_ids)?;
    let segment_serializer = SegmentSerializer::for_segment(merged_segment)?;
    let num_docs = merger.write(segment_serializer)?;

    let segment_meta = merged_index.new_segment_meta(merged_segment_id, num_docs);

    let stats = format!(
        "Segments Merge: [{}]",
        segments
            .iter()
            .fold(String::new(), |sum, current| format!(
                "{sum}{} ",
                current.meta().id().uuid_string()
            ))
            .trim_end()
    );

    let index_meta = IndexMeta {
        index_settings: target_settings,
        segments: vec![segment_meta],
        schema: target_schema,
        opstamp: 0u64,
        payload: Some(stats),
    };

    save_metas(&index_meta, merged_index.directory_mut())?;

    Ok(merged_index)
}

#[cfg(test)]
mod tests {
    use super::merge_indices;
    use crate::collector::TopDocs;
    use crate::directory::RamDirectory;
    use crate::fastfield::AliveBitSet;
    use crate::indexer::merge_policy::tests::MergeWheneverPossible;
    use crate::indexer::merger::IndexMerger;
    use crate::indexer::segment_updater::merge_filtered_segments;
    use crate::query::QueryParser;
    use crate::schema::*;
    use crate::{Directory, DocAddress, Index, Segment};

    #[test]
    fn test_delete_during_merge() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let text_field = schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());

        let mut index_writer = index.writer_for_tests()?;
        index_writer.set_merge_policy(Box::new(MergeWheneverPossible));

        for _ in 0..100 {
            index_writer.add_document(doc!(text_field=>"a"))?;
            index_writer.add_document(doc!(text_field=>"b"))?;
        }
        index_writer.commit()?;

        for _ in 0..100 {
            index_writer.add_document(doc!(text_field=>"c"))?;
            index_writer.add_document(doc!(text_field=>"d"))?;
        }
        index_writer.commit()?;

        index_writer.add_document(doc!(text_field=>"e"))?;
        index_writer.add_document(doc!(text_field=>"f"))?;
        index_writer.commit()?;

        let term = Term::from_field_text(text_field, "a");
        index_writer.delete_term(term);
        index_writer.commit()?;

        let reader = index.reader()?;
        assert_eq!(reader.searcher().num_docs(), 302);

        index_writer.wait_merging_threads()?;

        reader.reload()?;
        assert_eq!(reader.searcher().segment_readers().len(), 1);
        assert_eq!(reader.searcher().num_docs(), 302);
        Ok(())
    }

    #[test]
    fn delete_all_docs_min() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let text_field = schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());

        let mut index_writer = index.writer_for_tests()?;

        for _ in 0..10 {
            index_writer.add_document(doc!(text_field=>"a"))?;
            index_writer.add_document(doc!(text_field=>"b"))?;
        }
        index_writer.commit()?;

        let seg_ids = index.searchable_segment_ids()?;
        assert!(!seg_ids.is_empty());

        let term = Term::from_field_text(text_field, "a");
        index_writer.delete_term(term);
        index_writer.commit()?;

        let term = Term::from_field_text(text_field, "b");
        index_writer.delete_term(term);
        index_writer.commit()?;

        index_writer.wait_merging_threads()?;

        let reader = index.reader()?;
        assert_eq!(reader.searcher().num_docs(), 0);

        let seg_ids = index.searchable_segment_ids()?;
        assert!(seg_ids.is_empty());

        reader.reload()?;
        assert_eq!(reader.searcher().num_docs(), 0);
        assert!(index.searchable_segment_metas()?.is_empty());
        assert!(reader.searcher().segment_readers().is_empty());

        Ok(())
    }

    #[test]
    fn delete_all_docs() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let text_field = schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());

        let mut index_writer = index.writer_for_tests()?;

        for _ in 0..100 {
            index_writer.add_document(doc!(text_field=>"a"))?;
            index_writer.add_document(doc!(text_field=>"b"))?;
        }
        index_writer.commit()?;

        for _ in 0..100 {
            index_writer.add_document(doc!(text_field=>"c"))?;
            index_writer.add_document(doc!(text_field=>"d"))?;
        }
        index_writer.commit()?;

        index_writer.add_document(doc!(text_field=>"e"))?;
        index_writer.add_document(doc!(text_field=>"f"))?;
        index_writer.commit()?;

        let seg_ids = index.searchable_segment_ids()?;
        assert!(!seg_ids.is_empty());

        let term_vals = vec!["a", "b", "c", "d", "e", "f"];
        for term_val in term_vals {
            let term = Term::from_field_text(text_field, term_val);
            index_writer.delete_term(term);
            index_writer.commit()?;
        }

        index_writer.wait_merging_threads()?;

        let reader = index.reader()?;
        assert_eq!(reader.searcher().num_docs(), 0);

        let seg_ids = index.searchable_segment_ids()?;
        assert!(seg_ids.is_empty());

        reader.reload()?;
        assert_eq!(reader.searcher().num_docs(), 0);
        assert!(index.searchable_segment_metas()?.is_empty());
        assert!(reader.searcher().segment_readers().is_empty());

        Ok(())
    }

    #[test]
    fn test_remove_all_segments() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let text_field = schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());

        let mut index_writer = index.writer_for_tests()?;
        for _ in 0..100 {
            index_writer.add_document(doc!(text_field=>"a"))?;
            index_writer.add_document(doc!(text_field=>"b"))?;
        }
        index_writer.commit()?;

        index_writer.segment_updater().remove_all_segments();
        let seg_vec = index_writer
            .segment_updater()
            .segment_manager
            .segment_entries();
        assert!(seg_vec.is_empty());
        Ok(())
    }

    #[test]
    fn test_merge_segments() -> crate::Result<()> {
        let mut indices = vec![];
        let mut schema_builder = Schema::builder();
        let text_field = schema_builder.add_text_field("text", TEXT);
        let schema = schema_builder.build();

        for _ in 0..3 {
            let index = Index::create_in_ram(schema.clone());

            let mut index_writer = index.writer_for_tests()?;
            for _ in 0..100 {
                index_writer.add_document(doc!(text_field=>"fizz"))?;
                index_writer.add_document(doc!(text_field=>"buzz"))?;
            }
            index_writer.commit()?;

            for _ in 0..1000 {
                index_writer.add_document(doc!(text_field=>"foo"))?;
                index_writer.add_document(doc!(text_field=>"bar"))?;
            }
            index_writer.commit()?;
            indices.push(index);
        }

        assert_eq!(indices.len(), 3);
        let output_directory: Box<dyn Directory> = Box::<RamDirectory>::default();
        let index = merge_indices(&indices, output_directory)?;
        assert_eq!(index.schema(), schema);

        let segments = index.searchable_segments()?;
        assert_eq!(segments.len(), 1);

        let segment_metas = segments[0].meta();
        assert_eq!(segment_metas.num_deleted_docs(), 0);
        assert_eq!(segment_metas.num_docs(), 6600);
        Ok(())
    }

    #[test]
    fn test_merge_empty_indices_array() {
        let merge_result = merge_indices(&[], RamDirectory::default());
        assert!(merge_result.is_err());
    }

    #[test]
    fn test_merge_mismatched_schema() -> crate::Result<()> {
        let first_index = {
            let mut schema_builder = Schema::builder();
            let text_field = schema_builder.add_text_field("text", TEXT);
            let index = Index::create_in_ram(schema_builder.build());
            let mut index_writer = index.writer_for_tests()?;
            index_writer.add_document(doc!(text_field=>"some text"))?;
            index_writer.commit()?;
            index
        };

        let second_index = {
            let mut schema_builder = Schema::builder();
            let body_field = schema_builder.add_text_field("body", TEXT);
            let index = Index::create_in_ram(schema_builder.build());
            let mut index_writer = index.writer_for_tests()?;
            index_writer.add_document(doc!(body_field=>"some body"))?;
            index_writer.commit()?;
            index
        };

        let result = merge_indices(&[first_index, second_index], RamDirectory::default());
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn test_merge_filtered_segments() -> crate::Result<()> {
        let first_index = {
            let mut schema_builder = Schema::builder();
            let text_field = schema_builder.add_text_field("text", TEXT);
            let index = Index::create_in_ram(schema_builder.build());
            let mut index_writer = index.writer_for_tests()?;
            index_writer.add_document(doc!(text_field=>"some text 1"))?;
            index_writer.add_document(doc!(text_field=>"some text 2"))?;
            index_writer.commit()?;
            index
        };

        let second_index = {
            let mut schema_builder = Schema::builder();
            let text_field = schema_builder.add_text_field("text", TEXT);
            let index = Index::create_in_ram(schema_builder.build());
            let mut index_writer = index.writer_for_tests()?;
            index_writer.add_document(doc!(text_field=>"some text 3"))?;
            index_writer.add_document(doc!(text_field=>"some text 4"))?;
            index_writer.delete_term(Term::from_field_text(text_field, "4"));

            index_writer.commit()?;
            index
        };

        let mut segments: Vec<Segment> = Vec::new();
        segments.extend(first_index.searchable_segments()?);
        segments.extend(second_index.searchable_segments()?);

        let target_settings = first_index.settings().clone();

        let filter_segment_1 = AliveBitSet::for_test_from_deleted_docs(&[1], 2);
        let filter_segment_2 = AliveBitSet::for_test_from_deleted_docs(&[0], 2);

        let filter_segments = vec![Some(filter_segment_1), Some(filter_segment_2)];

        let merged_index = merge_filtered_segments(
            &segments,
            target_settings,
            filter_segments,
            RamDirectory::default(),
        )?;

        let segments = merged_index.searchable_segments()?;
        assert_eq!(segments.len(), 1);

        let segment_metas = segments[0].meta();
        assert_eq!(segment_metas.num_deleted_docs(), 0);
        assert_eq!(segment_metas.num_docs(), 1);

        Ok(())
    }

    #[test]
    fn test_merge_single_filtered_segments() -> crate::Result<()> {
        let first_index = {
            let mut schema_builder = Schema::builder();
            let text_field = schema_builder.add_text_field("text", TEXT);
            let index = Index::create_in_ram(schema_builder.build());
            let mut index_writer = index.writer_for_tests()?;
            index_writer.add_document(doc!(text_field=>"test text"))?;
            index_writer.add_document(doc!(text_field=>"some text 2"))?;

            index_writer.add_document(doc!(text_field=>"some text 3"))?;
            index_writer.add_document(doc!(text_field=>"some text 4"))?;

            index_writer.delete_term(Term::from_field_text(text_field, "4"));

            index_writer.commit()?;
            index
        };

        let mut segments: Vec<Segment> = Vec::new();
        segments.extend(first_index.searchable_segments()?);

        let target_settings = first_index.settings().clone();

        let filter_segment = AliveBitSet::for_test_from_deleted_docs(&[0], 4);

        let filter_segments = vec![Some(filter_segment)];

        let index = merge_filtered_segments(
            &segments,
            target_settings,
            filter_segments,
            RamDirectory::default(),
        )?;

        let segments = index.searchable_segments()?;
        assert_eq!(segments.len(), 1);

        let segment_metas = segments[0].meta();
        assert_eq!(segment_metas.num_deleted_docs(), 0);
        assert_eq!(segment_metas.num_docs(), 2);

        let searcher = index.reader()?.searcher();
        {
            let text_field = index.schema().get_field("text").unwrap();

            let do_search = |term: &str| {
                let query = QueryParser::for_index(&index, vec![text_field])
                    .parse_query(term)
                    .unwrap();
                let top_docs: Vec<(f32, DocAddress)> = searcher
                    .search(&query, &TopDocs::with_limit(3).order_by_score())
                    .unwrap();

                top_docs.iter().map(|el| el.1.doc_id).collect::<Vec<_>>()
            };

            assert_eq!(do_search("test"), vec![] as Vec<u32>);
            assert_eq!(do_search("text"), vec![0, 1]);
        }

        Ok(())
    }

    #[test]
    fn test_apply_doc_id_filter_in_merger() -> crate::Result<()> {
        let first_index = {
            let mut schema_builder = Schema::builder();
            let text_field = schema_builder.add_text_field("text", TEXT);
            let index = Index::create_in_ram(schema_builder.build());
            let mut index_writer = index.writer_for_tests()?;
            index_writer.add_document(doc!(text_field=>"some text 1"))?;
            index_writer.add_document(doc!(text_field=>"some text 2"))?;

            index_writer.add_document(doc!(text_field=>"some text 3"))?;
            index_writer.add_document(doc!(text_field=>"some text 4"))?;

            index_writer.delete_term(Term::from_field_text(text_field, "4"));

            index_writer.commit()?;
            index
        };

        let mut segments: Vec<Segment> = Vec::new();
        segments.extend(first_index.searchable_segments()?);

        let target_settings = first_index.settings().clone();
        {
            let filter_segment = AliveBitSet::for_test_from_deleted_docs(&[1], 4);
            let filter_segments = vec![Some(filter_segment)];
            let target_schema = segments[0].schema();
            let merged_index = Index::create(
                RamDirectory::default(),
                target_schema,
                target_settings.clone(),
            )?;
            let merger: IndexMerger = IndexMerger::open_with_custom_alive_set(
                merged_index.schema(),
                &segments[..],
                filter_segments,
            )?;

            let doc_ids_alive: Vec<_> = merger.readers[0].doc_ids_alive().collect();
            assert_eq!(doc_ids_alive, vec![0, 2]);
        }

        {
            let filter_segments = vec![None];
            let target_schema = segments[0].schema();
            let merged_index =
                Index::create(RamDirectory::default(), target_schema, target_settings)?;
            let merger: IndexMerger = IndexMerger::open_with_custom_alive_set(
                merged_index.schema(),
                &segments[..],
                filter_segments,
            )?;

            let doc_ids_alive: Vec<_> = merger.readers[0].doc_ids_alive().collect();
            assert_eq!(doc_ids_alive, vec![0, 1, 2]);
        }

        Ok(())
    }
}
