//! Honest query warnings.
//!
//! Every known limitation of the SFX v3 pipeline can be recognised from the
//! query itself (and a few facts about the index) before it runs. This module
//! turns those into plain-text warnings a caller can show next to the results:
//! what the engine will actually search, and where it has to fall back to
//! brute force.
//!
//! Pure functions over `QueryConfig`; no I/O. `index_warnings` needs a
//! searcher and is called by the handles.

use crate::query::QueryConfig;
use ld_lucivy::suffix_fst::briques::regex_verified;
use ld_lucivy::tokenizer::equal_chunk::is_content_char;

/// Longest query the briques orchestrator accepts (mirrors
/// `briques::orchestrator::MAX_QUERY_LEN`); anything longer returns nothing.
const MAX_QUERY_LEN: usize = 2048;

/// Below this many content bytes a literal hits a large share of any corpus
/// (`inc` = 40 688 of 50 000 kernel files) and the prescan, not the walk,
/// dominates.
const SHORT_LITERAL: usize = 3;

/// Warnings for one query, sub-queries included.
pub fn query_warnings(config: &QueryConfig) -> Vec<String> {
    let mut out = Vec::new();
    collect(config, &mut out);
    out.dedup();
    out
}

fn collect(config: &QueryConfig, out: &mut Vec<String>) {
    for list in [&config.must, &config.should, &config.must_not, &config.queries] {
        if let Some(subs) = list {
            for sub in subs {
                collect(sub, out);
            }
        }
    }

    let value = config.value.as_deref().or(config.pattern.as_deref()).unwrap_or("");
    match config.query_type.as_str() {
        "regex" => regex_warnings(value, out),
        "contains" | "sfx_contains" if config.regex == Some(true) => regex_warnings(value, out),
        "fuzzy" => fuzzy_warnings(value, config.distance.unwrap_or(1), out),
        "contains" | "sfx_contains" | "phrase" | "startsWith" | "term" | "parse"
        | "phrase_prefix" | "contains_split" | "sfx_contains_split" | "startsWith_split" => {
            let distance = config.distance.unwrap_or(0);
            if distance > 0 {
                fuzzy_warnings(value, distance, out);
            } else {
                let strict = config.strict_separators.unwrap_or(false);
                contains_warnings(value, strict, out);
            }
        }
        _ => {}
    }
}

fn stripped(value: &str) -> String {
    value.chars().filter(|c| is_content_char(*c)).collect()
}

fn contains_warnings(value: &str, strict: bool, out: &mut Vec<String>) {
    if value.is_empty() {
        return;
    }
    if value.len() > MAX_QUERY_LEN {
        out.push(format!(
            "query is {} bytes, above the {} byte limit: it returns nothing",
            value.len(), MAX_QUERY_LEN));
        return;
    }
    let content = stripped(value);
    if !strict {
        if content.is_empty() {
            out.push(format!(
                "{value:?} is only separators and separators are ignored \
                 (strict_separators=false): it returns nothing"));
            return;
        }
        if content != value {
            out.push(format!(
                "separators are ignored (strict_separators=false): {value:?} is searched as {content:?}"));
        }
        if content.len() < SHORT_LITERAL {
            out.push(format!(
                "{content:?} is shorter than {SHORT_LITERAL} bytes: most documents will match, \
                 cost grows with corpus size"));
        }
    } else if content.is_empty() {
        out.push(format!(
            "{value:?} is only separators: every occurrence of the sequence is returned, \
             which can be millions of spans on source code"));
    } else if value.len() < SHORT_LITERAL {
        out.push(format!(
            "{value:?} is shorter than {SHORT_LITERAL} bytes: most documents will match, \
             cost grows with corpus size"));
    }
}

fn fuzzy_warnings(value: &str, distance: u8, out: &mut Vec<String>) {
    if value.is_empty() {
        return;
    }
    let content = stripped(value);
    if content.is_empty() {
        out.push(format!(
            "{value:?} is only separators and fuzzy search ignores them: it returns nothing"));
        return;
    }
    if content != value {
        out.push(format!(
            "fuzzy search ignores separators: {value:?} is searched as {content:?}"));
    }
    let chars = content.chars().count();
    // `init` at distance 1 also matches `int`, `unit`, `inet` (44 579 of
    // 50 000 kernel files): one edit on four characters is already too loose.
    if chars <= 3 * distance as usize + 1 {
        out.push(format!(
            "distance {distance} on {content:?} ({chars} chars) rewrites a quarter of the query \
             or more: unrelated short words will match (e.g. `init` at distance 1 also matches \
             `int`, `unit`)"));
    }
    if distance > 3 {
        out.push(format!(
            "distance {distance}: the candidate generator is tuned for distances 1-3, \
             larger distances cost much more"));
    }
}

fn regex_warnings(pattern: &str, out: &mut Vec<String>) {
    if pattern.is_empty() {
        return;
    }
    let Some(plan) = regex_verified::plan(pattern) else {
        out.push(format!("{pattern:?} is not a valid regex: it returns nothing"));
        return;
    };
    if plan.literals.is_empty() {
        out.push(format!(
            "{pattern:?} requires no literal the index can look up: every document is \
             scanned whole (full scan, cost grows with corpus size)"));
        return;
    }
    let shortest = plan.literals.iter().map(|l| l.len()).min().unwrap_or(0);
    if shortest < SHORT_LITERAL {
        out.push(format!(
            "{pattern:?} is located through literals as short as {shortest} byte(s): \
             most documents are candidates and are scanned"));
    }
    if plan.max_len.is_none() {
        out.push(format!(
            "{pattern:?} has no bounded match length: candidate documents are scanned \
             whole instead of a window around each literal"));
    }
}

/// Index-level warnings: segments the v3 pipeline cannot serve.
///
/// `sfx_versions` is one entry per SFX segment file, as returned by
/// `detect_sfx_version`. Anything but `Some(3)` goes through the legacy
/// pipeline, whose spans and modes differ from what this crate documents.
pub fn index_warnings(sfx_versions: &[Option<u8>]) -> Vec<String> {
    let legacy = sfx_versions.iter().filter(|v| **v != Some(3)).count();
    if legacy == 0 {
        return Vec::new();
    }
    vec![format!(
        "{legacy} of {} SFX segment file(s) were written by the v2 indexer: relaxed, fuzzy \
         and regex spans on those segments come from the legacy pipeline (reindex to fix)",
        sfx_versions.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(t: &str, v: &str) -> QueryConfig {
        QueryConfig { query_type: t.into(), value: Some(v.into()), ..Default::default() }
    }

    #[test]
    fn clean_queries_are_silent() {
        assert!(query_warnings(&q("contains", "kmalloc")).is_empty());
        assert!(query_warnings(&q("fuzzy", "kmalloc")).is_empty());
        assert!(query_warnings(&q("regex", r"kmalloc\(")).is_empty());
        let mut strict = q("contains", "spin_lock");
        strict.strict_separators = Some(true);
        assert!(query_warnings(&strict).is_empty());
    }

    #[test]
    fn relaxed_separators_are_reported() {
        let w = query_warnings(&q("contains", "__init"));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("\"init\""));
        let w = query_warnings(&q("contains", "->"));
        assert!(w[0].contains("returns nothing"));
    }

    #[test]
    fn strict_separators_only_is_a_cost_warning() {
        let mut c = q("contains", "\t\t");
        c.strict_separators = Some(true);
        let w = query_warnings(&c);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("millions"));
    }

    #[test]
    fn fuzzy_half_rewrite() {
        let w = query_warnings(&q("fuzzy", "init"));
        assert!(w.iter().any(|m| m.contains("quarter")), "{w:?}");
        assert!(query_warnings(&q("fuzzy", "kmalloc")).is_empty());
        // Both limits of the comment-regex panel query are real: 2-byte
        // literal and unbounded length, 29 381 documents scanned whole.
        assert_eq!(query_warnings(&q("regex", r"/\*[^*]*\*/")).len(), 2);
        let mut c = q("fuzzy", "__init");
        c.distance = Some(1);
        let w = query_warnings(&c);
        assert!(w.iter().any(|m| m.contains("ignores separators")));
    }

    #[test]
    fn regex_full_scan_and_unbounded() {
        let w = query_warnings(&q("regex", "[0-9]{8}"));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("full scan"));
        let w = query_warnings(&q("regex", "include[a-z]*"));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("no bounded match length"));
        let w = query_warnings(&q("regex", "a("));
        assert!(w[0].contains("not a valid regex"));
    }

    #[test]
    fn boolean_recurses_and_dedups() {
        let c = QueryConfig {
            query_type: "boolean".into(),
            should: Some(vec![q("contains", "__init"), q("contains", "__init"), q("regex", "[0-9]{8}")]),
            ..Default::default()
        };
        let w = query_warnings(&c);
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn index_legacy_segments() {
        assert!(index_warnings(&[Some(3), Some(3)]).is_empty());
        let w = index_warnings(&[Some(3), Some(2), None]);
        assert!(w[0].starts_with("2 of 3"));
    }
}
