//! **Measure, ignored: find words an index lost** (20 November 2026, before rebuilding rag3weaver's code index). A
//! lost word is an id a live segment's `.gmap` cites and no dictionary generation holds a text for: its text is gone,
//! so it cannot be printed. This walk finds them by their documents: in each segment that cites a lost id, it reads
//! the live documents' stored `content`, cuts it into words, and searches each word (`contains`, distance 0); a
//! document that holds a word and does not come back for it lost that word. Prints up to `LUCIVY_LOST_MAX` (5) words
//! with their document's `_node_id`. Point `LUCIVY_DICT_INDEX` at a **copy**.

use std::collections::HashSet;

use ld_lucivy::schema::document::Value;
use ld_lucivy::suffix_fst::gmap::GmapReader;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;

#[test]
#[ignore]
fn find_words_an_index_lost() {
    let path = std::env::var("LUCIVY_DICT_INDEX").expect("LUCIVY_DICT_INDEX=<a copy of a sharded index>");
    let max: usize = std::env::var("LUCIVY_LOST_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let index = ShardedHandle::open(&path).unwrap();
    let content = index.field("content").expect("content");
    let mut found = 0;
    let mut shard_id = 0;
    'shards: while let Some(shard) = index.shard(shard_id) {
        shard_id += 1;
        let dictionary = shard.index.sfx_dictionary().expect("dictionary");
        let texts = dictionary.field(content.field_id()).expect("field").termtexts().expect("texts");
        let held: HashSet<u32> = texts.iter().map(|e| e.0).collect();
        let nid_f = shard.field(NODE_ID_FIELD).expect("_node_id");
        let searcher = shard.reader.searcher();
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            let Some(file) = segment.sfx_index_file("gmap", content) else { continue };
            let bytes = file.read_bytes().unwrap();
            let lost = GmapReader::open(bytes.as_slice()).unwrap().iter().filter(|id| !held.contains(id)).count();
            if lost == 0 {
                continue;
            }
            println!("shard {} segment {}: {lost} lost ids of content, {} docs", shard_id - 1, segment.segment_id().short_uuid_string(), segment.max_doc());
            for doc_id in 0..segment.max_doc() {
                if segment.is_deleted(doc_id) {
                    continue;
                }
                let doc = searcher.doc::<ld_lucivy::LucivyDocument>(ld_lucivy::DocAddress::new(ord as u32, doc_id)).unwrap();
                let nid = doc.field_values().find(|(f, _)| *f == nid_f).and_then(|(_, v)| v.as_value().as_u64()).unwrap_or(u64::MAX);
                let text: String = doc.field_values().filter(|(f, _)| *f == content).filter_map(|(_, v)| v.as_value().as_str().map(str::to_string)).collect::<Vec<_>>().join("\n");
                let mut words: Vec<&str> = text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.len() >= 5).collect();
                words.sort_unstable();
                words.dedup();
                for word in words {
                    let q = QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(word.into()), distance: Some(0), ..Default::default() };
                    let results = index.search(&q, 100_000, None).unwrap();
                    let ids = index.node_ids_of(&results).unwrap();
                    if !ids.contains(&nid) {
                        println!("LOST word {word:?} in document _node_id {nid} (shard {}, segment {}), {} other documents found for it", shard_id - 1, segment.segment_id().short_uuid_string(), ids.len());
                        found += 1;
                        if found >= max {
                            break 'shards;
                        }
                        break;
                    }
                }
            }
        }
    }
    println!("{found} lost word(s) found");
}
