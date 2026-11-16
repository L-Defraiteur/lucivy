//! The entry used by the real derived-in-RAM consumer and its lightweight witness.
//! Baseline only: loading/UTF-8 failures propagate, but panel coverage is NOT checked yet.

use lucivy_core::query::QueryConfig;

pub type LoadedCorpus = Vec<(String, Vec<u8>)>;
pub type Corpus = Vec<(String, String)>;

#[derive(Debug, PartialEq)]
pub enum FixtureError {
    Read { source: String, reason: String },
    Utf8 { source: String, reason: String },
    UncoveredPanel { queries: Vec<String> },
}

pub struct PanelQuery {
    pub label: &'static str,
    pub config: QueryConfig,
}

pub fn panel() -> Vec<PanelQuery> {
    let q = |text: &str, label, strict, distance, anchor, exact| PanelQuery {
        label,
        config: QueryConfig {
            query_type: "contains".into(), field: Some("content".into()), value: Some(text.into()),
            strict_separators: Some(strict),
            distance: if distance > 0 && distance != 255 { Some(distance) } else { None },
            regex: if distance == 255 { Some(true) } else { None },
            anchor_start: if anchor { Some(true) } else { None },
            exact_match: if exact { Some(true) } else { None },
            ..Default::default()
        },
    };
    vec![
        q("mutex_lock", "strict", true, 0, false, false),
        q("mutex lock", "relax", false, 0, false, false),
        q("spin_lock", "strict", true, 0, false, false),
        q("sched", "term", true, 0, true, true),
        q("printk", "sw", true, 0, true, false),
        q("return", "relax", false, 0, false, false),
        q("if (", "strict", true, 0, false, false),
        q("schdule", "fz1", false, 1, false, false),
        q("regsiter", "fz2", false, 2, false, false),
        q("spin_lock_[a-z]+", "rx", true, 255, false, false),
        q("Mutex", "relax-case", false, 0, false, false),
    ]
}

pub fn with_derived_fixture<T>(
    load: impl FnOnce() -> Result<LoadedCorpus, FixtureError>,
    build: impl FnOnce(&Corpus, &[PanelQuery]) -> T,
) -> Result<T, FixtureError> {
    let files = load()?.into_iter().map(|(source, bytes)| {
        String::from_utf8(bytes).map(|text| (source.clone(), text))
            .map_err(|error| FixtureError::Utf8 { source, reason: error.to_string() })
    }).collect::<Result<Corpus, _>>()?;
    // Preserve the old consumer's missing coverage gate for the real red witness.
    // This is deliberately not the correction: the builder is still called.
    Ok(build(&files, &panel()))
}
