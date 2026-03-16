//! Aggregated BM25 statistics across multiple shards.
//!
//! Implements `Bm25StatisticsProvider` by summing stats from N searchers.
//! Used by `ShardedHandle` to provide consistent cross-shard scoring.
//!
//! Also usable by rag3weaver for super-sharding (cross-entity aggregation).

use ld_lucivy::query::Bm25StatisticsProvider;
use ld_lucivy::schema::{Field, Term};
use ld_lucivy::Searcher;

/// Aggregated BM25 statistics provider (borrowed version).
///
/// Wraps N `Searcher` references and computes global stats by summing
/// each shard's local stats.
pub struct AggregatedBm25Stats<'a> {
    searchers: Vec<&'a Searcher>,
}

impl<'a> AggregatedBm25Stats<'a> {
    /// Create from a slice of searcher references.
    pub fn new(searchers: Vec<&'a Searcher>) -> Self {
        Self { searchers }
    }
}

impl<'a> Bm25StatisticsProvider for AggregatedBm25Stats<'a> {
    fn total_num_tokens(&self, field: Field) -> ld_lucivy::Result<u64> {
        let mut total = 0u64;
        for searcher in &self.searchers {
            total += searcher.total_num_tokens(field)?;
        }
        Ok(total)
    }

    fn total_num_docs(&self) -> ld_lucivy::Result<u64> {
        let mut total = 0u64;
        for searcher in &self.searchers {
            total += searcher.total_num_docs()?;
        }
        Ok(total)
    }

    fn doc_freq(&self, term: &Term) -> ld_lucivy::Result<u64> {
        let mut total = 0u64;
        for searcher in &self.searchers {
            total += searcher.doc_freq(term)?;
        }
        Ok(total)
    }
}

/// Owned version — wraps N `Searcher` (owned) for cross-shard BM25.
///
/// `Send + Sync` so it can be shared via `Arc` across actor messages.
/// Used by `ShardedHandle` to inject global stats into per-shard searches.
pub struct AggregatedBm25StatsOwned {
    searchers: Vec<Searcher>,
}

impl AggregatedBm25StatsOwned {
    /// Create from owned searchers (one per shard).
    pub fn new(searchers: Vec<Searcher>) -> Self {
        Self { searchers }
    }
}

impl Bm25StatisticsProvider for AggregatedBm25StatsOwned {
    fn total_num_tokens(&self, field: Field) -> ld_lucivy::Result<u64> {
        let mut total = 0u64;
        for searcher in &self.searchers {
            total += searcher.total_num_tokens(field)?;
        }
        Ok(total)
    }

    fn total_num_docs(&self) -> ld_lucivy::Result<u64> {
        let mut total = 0u64;
        for searcher in &self.searchers {
            total += searcher.total_num_docs()?;
        }
        Ok(total)
    }

    fn doc_freq(&self, term: &Term) -> ld_lucivy::Result<u64> {
        let mut total = 0u64;
        for searcher in &self.searchers {
            total += searcher.doc_freq(term)?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::StdFsDirectory;
    use crate::handle::{LucivyHandle, NODE_ID_FIELD};
    use crate::query::SchemaConfig;

    #[test]
    fn test_aggregated_stats_single_shard() {
        let tmp = std::env::temp_dir().join("lucivy_bm25_global_single");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        }))
        .unwrap();

        let dir = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut guard = handle.writer.lock().unwrap();
            let w = guard.as_mut().unwrap();
            for i in 0u64..10 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("word_{i} common"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        let searcher = handle.reader.searcher();
        let stats = AggregatedBm25Stats::new(vec![&searcher]);

        // total_num_docs should be 10
        assert_eq!(stats.total_num_docs().unwrap(), 10);

        // total_num_tokens should be > 0
        assert!(stats.total_num_tokens(body).unwrap() > 0);

        // doc_freq for "common" should be 10
        let term = Term::from_field_text(body, "common");
        assert_eq!(stats.doc_freq(&term).unwrap(), 10);
    }

    #[test]
    fn test_aggregated_stats_two_shards() {
        let tmp1 = std::env::temp_dir().join("lucivy_bm25_global_s1");
        let tmp2 = std::env::temp_dir().join("lucivy_bm25_global_s2");
        let _ = std::fs::remove_dir_all(&tmp1);
        let _ = std::fs::remove_dir_all(&tmp2);
        std::fs::create_dir_all(&tmp1).unwrap();
        std::fs::create_dir_all(&tmp2).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        }))
        .unwrap();

        // Shard 1: 10 docs with "common"
        let dir1 = StdFsDirectory::open(tmp1.to_str().unwrap()).unwrap();
        let h1 = LucivyHandle::create(dir1, &config).unwrap();
        let body1 = h1.field("body").unwrap();
        let nid1 = h1.field(NODE_ID_FIELD).unwrap();
        {
            let mut g = h1.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 0u64..10 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid1, i);
                doc.add_text(body1, &format!("shard1 word_{i} common"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        h1.reader.reload().unwrap();

        // Shard 2: 5 docs with "common"
        let dir2 = StdFsDirectory::open(tmp2.to_str().unwrap()).unwrap();
        let h2 = LucivyHandle::create(dir2, &config).unwrap();
        let body2 = h2.field("body").unwrap();
        let nid2 = h2.field(NODE_ID_FIELD).unwrap();
        {
            let mut g = h2.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 0u64..5 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid2, 100 + i);
                doc.add_text(body2, &format!("shard2 word_{i} common"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        h2.reader.reload().unwrap();

        let s1 = h1.reader.searcher();
        let s2 = h2.reader.searcher();
        let stats = AggregatedBm25Stats::new(vec![&s1, &s2]);

        // 10 + 5 = 15
        assert_eq!(stats.total_num_docs().unwrap(), 15);

        // doc_freq("common") = 10 + 5 = 15
        let term = Term::from_field_text(body1, "common");
        assert_eq!(stats.doc_freq(&term).unwrap(), 15);

        // doc_freq("shard1") = 10 (only in shard 1)
        let term_s1 = Term::from_field_text(body1, "shard1");
        assert_eq!(stats.doc_freq(&term_s1).unwrap(), 10);

        // doc_freq("shard2") = 5 (only in shard 2)
        let term_s2 = Term::from_field_text(body1, "shard2");
        assert_eq!(stats.doc_freq(&term_s2).unwrap(), 5);
    }
}
