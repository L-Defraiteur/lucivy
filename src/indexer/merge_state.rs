use crate::fieldnorm::FieldNormReaders;
use crate::index::{Index, Segment, SegmentComponent};
use crate::indexer::doc_id_mapping::SegmentDocIdMapping;
use crate::indexer::merger::IndexMerger;
use crate::indexer::segment_entry::SegmentEntry;
use crate::indexer::segment_serializer::SegmentSerializer;
use crate::indexer::delete_queue::DeleteCursor;
use crate::schema::Field;
use crate::Opstamp;

/// Résultat d'un step du merge incrémental.
pub(crate) enum StepResult {
    /// Budget épuisé ou phase terminée, rappeler pour continuer.
    Continue,
    /// Merge terminé. Contient le SegmentEntry résultant (None si 0 docs).
    Done(Option<SegmentEntry>),
}

/// Phase courante du merge incrémental.
enum MergePhase {
    /// Calcul du mapping doc IDs + écriture fieldnorms.
    Init,
    /// Écriture des postings, un champ indexé par step.
    Postings { field_idx: usize },
    /// Écriture du document store.
    Store,
    /// Écriture des fast fields (colonnes).
    FastFields,
    /// Merge des fichiers .sfx (suffix FST).
    Sfx,
    /// Finalisation : close serializer + construction du résultat.
    Close,
}

/// État d'un merge incrémental.
///
/// Encapsule tout le state nécessaire pour exécuter un merge en steps
/// via `poll_idle()` du SegmentUpdaterActor. Chaque appel à `step()`
/// avance d'une phase ou d'un champ, puis rend la main au scheduler.
///
/// Granularité : un step = une phase (Init, Store, FastFields, Close)
/// ou un champ indexé (pendant la phase Postings). La phase Postings
/// est la plus coûteuse — yield entre chaque champ permet au scheduler
/// d'intercaler les messages (AddSegment, Commit, etc.).
pub(crate) struct MergeState {
    index: Index,
    merger: IndexMerger,
    serializer: Option<SegmentSerializer>,
    merged_segment: Segment,
    delete_cursor: DeleteCursor,
    doc_id_mapping: Option<SegmentDocIdMapping>,
    /// Saved for .sfx merge (doc_id_mapping is consumed by fast fields).
    sfx_doc_mapping: Option<Vec<crate::DocAddress>>,
    fieldnorm_readers: Option<FieldNormReaders>,
    phase: MergePhase,
    /// Liste pré-calculée des champs indexés (Field + index dans le schéma).
    indexed_fields: Vec<Field>,
    /// Compteur de steps complétés (pour observabilité).
    steps_completed: u32,
    /// Timing: when this merge started.
    merge_start: std::time::Instant,
    /// Timing: phase start.
    phase_start: std::time::Instant,
    /// Total docs in this merge.
    total_docs: u32,
}

impl MergeState {
    /// Crée un nouveau MergeState prêt à être steppé.
    ///
    /// Retourne `Ok(None)` si tous les segments sont vides (rien à merger).
    /// Fait les préparatifs : advance_deletes, création du segment cible,
    /// ouverture du merger et du serializer.
    pub fn new(
        index: &Index,
        mut segment_entries: Vec<SegmentEntry>,
        target_opstamp: Opstamp,
    ) -> crate::Result<Option<Self>> {
        let num_docs: u64 = segment_entries
            .iter()
            .map(|s| s.meta().num_docs() as u64)
            .sum();
        if num_docs == 0 {
            return Ok(None);
        }

        let merged_segment = index.new_segment();

        for segment_entry in &mut segment_entries {
            let segment = index.segment(segment_entry.meta().clone());
            crate::indexer::index_writer::advance_deletes(
                segment,
                segment_entry,
                target_opstamp,
            )?;
        }

        let delete_cursor = segment_entries[0].delete_cursor().clone();

        let segments: Vec<Segment> = segment_entries
            .iter()
            .map(|se| index.segment(se.meta().clone()))
            .collect();

        let schema = index.schema();
        let merger = IndexMerger::open(schema.clone(), &segments[..])?;
        let serializer = SegmentSerializer::for_segment(merged_segment.clone())?;

        // Pré-calculer la liste des champs indexés pour la phase Postings.
        let indexed_fields: Vec<Field> = schema
            .fields()
            .filter_map(|(field, entry)| {
                if entry.is_indexed() {
                    Some(field)
                } else {
                    None
                }
            })
            .collect();

        let total_docs: u32 = merger.readers.iter().map(|r| r.num_docs()).sum();
        let num_segments = merger.readers.len();
        lucivy_trace!("[merge] new: {} segments, {} docs total", num_segments, total_docs);

        Ok(Some(MergeState {
            index: index.clone(),
            merger,
            serializer: Some(serializer),
            merged_segment,
            delete_cursor,
            doc_id_mapping: None,
            sfx_doc_mapping: None,
            fieldnorm_readers: None,
            phase: MergePhase::Init,
            indexed_fields,
            steps_completed: 0,
            merge_start: std::time::Instant::now(),
            phase_start: std::time::Instant::now(),
            total_docs,
        }))
    }

    /// Avance le merge d'un step. Retourne `Continue` si le merge
    /// n'est pas terminé, `Done` quand il l'est.
    pub fn step(&mut self) -> StepResult {
        match self.do_step() {
            Ok(StepResult::Continue) => {
                self.steps_completed += 1;
                StepResult::Continue
            }
            Ok(done) => done,
            Err(e) => {
                // En cas d'erreur, on log et on signale Done(None).
                // Le caller (SegmentUpdaterActor) traitera ça comme un
                // merge échoué — les segments source restent inchangés.
                warn!("Incremental merge step failed: {e:?}");
                StepResult::Done(None)
            }
        }
    }

    fn do_step(&mut self) -> crate::Result<StepResult> {
        let phase_name = match &self.phase {
            MergePhase::Init => "init",
            MergePhase::Postings { field_idx } => "postings",
            MergePhase::Store => "store",
            MergePhase::FastFields => "fast_fields",
            MergePhase::Sfx => "sfx",
            MergePhase::Close => "close",
        };
        self.phase_start = std::time::Instant::now();

        let result = match self.phase {
            MergePhase::Init => self.step_init(),
            MergePhase::Postings { .. } => self.step_postings(),
            MergePhase::Store => self.step_store(),
            MergePhase::FastFields => self.step_fast_fields(),
            MergePhase::Sfx => self.step_sfx(),
            MergePhase::Close => {
                let r = self.step_close();
                lucivy_trace!("[merge] done: {} docs in {:.1}ms",
                    self.total_docs, self.merge_start.elapsed().as_secs_f64() * 1000.0);
                r
            }
        };

        let elapsed = self.phase_start.elapsed().as_secs_f64() * 1000.0;
        if elapsed > 10.0 {
            lucivy_trace!("[merge]   {} took {:.1}ms ({} docs)", phase_name, elapsed, self.total_docs);
        }
        result
    }

    /// Phase Init : calcul du doc ID mapping + écriture des fieldnorms.
    fn step_init(&mut self) -> crate::Result<StepResult> {
        let doc_id_mapping = self.merger.get_doc_id_from_concatenated_data()?;

        let serializer = self.serializer.as_mut().unwrap();
        if let Some(fieldnorms_serializer) = serializer.extract_fieldnorms_serializer() {
            self.merger
                .write_fieldnorms(fieldnorms_serializer, &doc_id_mapping)?;
        }

        let fieldnorm_data = serializer
            .segment()
            .open_read(SegmentComponent::FieldNorms)?;
        let fieldnorm_readers = FieldNormReaders::open(fieldnorm_data)?;

        self.doc_id_mapping = Some(doc_id_mapping);
        self.fieldnorm_readers = Some(fieldnorm_readers);
        self.phase = MergePhase::Postings { field_idx: 0 };
        Ok(StepResult::Continue)
    }

    /// Phase Postings : traite un champ indexé par step.
    fn step_postings(&mut self) -> crate::Result<StepResult> {
        let field_idx = match &mut self.phase {
            MergePhase::Postings { field_idx } => field_idx,
            _ => unreachable!(),
        };

        if *field_idx >= self.indexed_fields.len() {
            // Tous les champs traités → phase suivante.
            self.phase = MergePhase::Store;
            return Ok(StepResult::Continue);
        }

        let field = self.indexed_fields[*field_idx];
        let field_entry = self.merger.schema.get_field_entry(field);
        let fieldnorm_reader = self
            .fieldnorm_readers
            .as_ref()
            .unwrap()
            .get_field(field)?;

        let serializer = self.serializer.as_mut().unwrap();
        self.merger.write_postings_for_field(
            field,
            field_entry.field_type(),
            serializer.get_postings_serializer(),
            fieldnorm_reader,
            self.doc_id_mapping.as_ref().unwrap(),
        )?;

        *field_idx += 1;
        Ok(StepResult::Continue)
    }

    /// Phase Store : écriture du document store.
    fn step_store(&mut self) -> crate::Result<StepResult> {
        let serializer = self.serializer.as_mut().unwrap();
        self.merger
            .write_storable_fields(serializer.get_store_writer())?;
        self.phase = MergePhase::FastFields;
        Ok(StepResult::Continue)
    }

    /// Phase FastFields : écriture des colonnes (fast fields).
    fn step_fast_fields(&mut self) -> crate::Result<StepResult> {
        let serializer = self.serializer.as_mut().unwrap();
        let doc_id_mapping = self.doc_id_mapping.take().unwrap();
        // Save doc mapping for .sfx merge before it's consumed
        self.sfx_doc_mapping = Some(doc_id_mapping.iter_old_doc_addrs().collect());
        self.merger
            .write_fast_fields(serializer.get_fast_field_write(), doc_id_mapping)?;
        self.phase = MergePhase::Sfx;
        Ok(StepResult::Continue)
    }

    /// Phase Sfx : merge suffix FST data (always full FST).
    fn step_sfx(&mut self) -> crate::Result<StepResult> {
        let serializer = self.serializer.as_mut().unwrap();
        let doc_mapping = self.sfx_doc_mapping.take().unwrap_or_default();
        self.merger.merge_sfx(serializer, &doc_mapping)?;
        self.phase = MergePhase::Close;
        Ok(StepResult::Continue)
    }

    /// Phase Close : finalisation du serializer + construction du résultat.
    fn step_close(&mut self) -> crate::Result<StepResult> {
        let serializer = self.serializer.take().unwrap();
        serializer.close()?;

        let merged_segment_id = self.merged_segment.id();
        let num_docs = self.merger.max_doc;
        let segment_meta = self.index.new_segment_meta(merged_segment_id, num_docs);
        let entry = SegmentEntry::new(segment_meta, self.delete_cursor.clone(), None);

        Ok(StepResult::Done(Some(entry)))
    }

    /// Nombre total de steps estimé (pour observabilité).
    pub fn estimated_steps(&self) -> u32 {
        // Init + N champs indexés + Store + FastFields + Close
        (1 + self.indexed_fields.len() + 3) as u32
    }

    /// Nombre de steps complétés.
    pub fn steps_completed(&self) -> u32 {
        self.steps_completed
    }
}

/// Exécute un merge de manière synchrone en bouclant sur `MergeState::step()`.
///
/// C'est le wrapper synchrone qui remplace l'ancien `merge()` pour l'API
/// existante (IndexWriter::merge). Le comportement est identique à l'ancien
/// code, mais l'implémentation passe par la state machine.
#[allow(dead_code)]
pub(crate) fn merge_incremental(
    index: &Index,
    segment_entries: Vec<SegmentEntry>,
    target_opstamp: Opstamp,
) -> crate::Result<Option<SegmentEntry>> {
    let mut state = match MergeState::new(index, segment_entries, target_opstamp)? {
        Some(s) => s,
        None => return Ok(None),
    };

    loop {
        match state.step() {
            StepResult::Continue => continue,
            StepResult::Done(result) => return Ok(result),
        }
    }
}
