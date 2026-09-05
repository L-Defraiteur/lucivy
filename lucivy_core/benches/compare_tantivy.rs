//! tantivy against lucivy, on the same corpus, judged by the same grep.
//!
//! tantivy — not Elasticsearch — is the comparison that matters: it is a
//! library, in-process, embeddable, so "no server needed" is not an argument
//! against it. lucivy is a fork of it, which makes this the question a reader
//! will actually ask: what did the fork buy, and what did it cost.
//!
//! **tantivy is configured at its best, twice.** Once with the default
//! tokenizer, which is what anyone starts from, and once with an
//! `NgramTokenizer`, which is how tantivy does substring search. Comparing
//! only against the default would be comparing against a straw man, and the
//! first informed reader would say so.
//!
//! Every row carries what grep says is true, what each engine returned, how
//! long it took, and — separately — what it costs to learn *where* it matched,
//! since that is the result lucivy is built to give and the one that has to be
//! priced fairly on both sides.
//!
//! ```bash
//! CMP_CORPUS=/tmp/lucivy-cmp-90k cargo test --release -p lucivy-core \
//!     --test compare_tantivy -- --ignored --nocapture
//! ```

use std::path::Path;
use std::time::Instant;

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, FuzzyTermQuery, PhraseQuery, Query, QueryParser, RegexQuery, TermQuery};
use tantivy::collector::DocSetCollector;
use tantivy::schema::{
    IndexRecordOption, Schema as TvSchema, TextFieldIndexing, TextOptions, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::schema::Value;
use tantivy::{doc, Index as TvIndex, TantivyDocument, Term};

const MAX_FILE_SIZE: u64 = 100_000;

fn corpus_root() -> String {
    std::env::var("CMP_CORPUS").unwrap_or_else(|_| "/tmp/lucivy-cmp-90k".into())
}

/// The same selection as everywhere else in this comparison. The corpus is
/// materialised on disk beforehand precisely so this cannot drift: no
/// symlinks, no surprises, the same files for every engine.
fn collect(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    fn walk(dir: &Path, root: &Path, files: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, files);
            } else if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size == 0 || size > MAX_FILE_SIZE {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if bytes.contains(&0) {
                    continue;
                }
                let Ok(content) = String::from_utf8(bytes) else { continue };
                if content.trim().is_empty() {
                    continue;
                }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                files.push((rel, content));
            }
        }
    }
    walk(root, root, &mut files);
    files
}

fn dir_size(path: &str) -> u64 {
    fn walk(p: &Path) -> u64 {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    total += walk(&path);
                } else {
                    total += path.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        total
    }
    walk(Path::new(path))
}

struct Built {
    index: TvIndex,
    content: tantivy::schema::Field,
    seconds: f64,
    bytes: u64,
}

/// `ngram = false` is tantivy as it comes; `true` is tantivy set up to find
/// substrings, which is the only way it can. The trigram tokenizer runs over
/// the character stream, so it crosses the boundaries the default tokenizer
/// would cut — the same mechanism Elasticsearch uses.
fn build(files: &[(String, String)], dir: &str, ngram: bool) -> Built {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let mut sb = TvSchema::builder();
    sb.add_text_field("path", TEXT | STORED);
    let content = if ngram {
        let indexing = TextFieldIndexing::default()
            .set_tokenizer("tri")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        sb.add_text_field("content", TextOptions::default().set_indexing_options(indexing).set_stored())
    } else {
        sb.add_text_field("content", TEXT | STORED)
    };
    let schema = sb.build();
    let path_field = schema.get_field("path").unwrap();

    let index = TvIndex::create_in_dir(dir, schema).unwrap();
    if ngram {
        // Trigrams over the whole character stream, lowercased: `spin_lock`
        // stays findable inside `raw_spin_lock`.
        let analyzer = TextAnalyzer::builder(NgramTokenizer::new(3, 3, false).unwrap())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("tri", analyzer);
    }

    let t0 = Instant::now();
    let mut writer = index.writer(400_000_000).unwrap();
    for (p, c) in files {
        writer.add_document(doc!(path_field => p.as_str(), content => c.as_str())).unwrap();
    }
    writer.commit().unwrap();
    let seconds = t0.elapsed().as_secs_f64();
    drop(writer);

    Built { index, content, seconds, bytes: dir_size(dir) }
}

/// A phrase over the needle's trigrams, built by hand.
///
/// `QueryParser` on a trigram field does **not** give this: asked for
/// `"spin_lock"` it produced a query matching any document holding those
/// trigrams anywhere, which returned more hits for `spinlock` than for
/// `spin_lock` itself — impossible, and a reminder that a comparison is only
/// as good as the query it puts in the other engine's mouth. Positions are
/// what makes trigrams mean "this substring", so the phrase is assembled
/// explicitly here.
fn trigram_phrase(field: tantivy::schema::Field, needle: &str) -> Box<dyn Query> {
    let lower = needle.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let terms: Vec<(usize, Term)> = chars
        .windows(3)
        .enumerate()
        .map(|(i, w)| (i, Term::from_field_text(field, &w.iter().collect::<String>())))
        .collect();
    // Three characters make one trigram: a phrase of one term is refused by
    // tantivy, a term query asks the same thing.
    if terms.len() == 1 {
        return Box::new(TermQuery::new(terms[0].1.clone(), IndexRecordOption::WithFreqsAndPositions));
    }
    Box::new(PhraseQuery::new_with_offset(terms))
}

/// The honest path for a substring on tantivy's trigram index. Its
/// `NgramTokenizer` emits every position as 0 (their source says so: "With
/// this tokenizer, the `position` is always 0"), so a phrase over trigrams
/// matches nothing — and `trigram_phrase` above returns 0 on every needle. What
/// an application can do is what lucivy does inside: take the documents that
/// hold **all** the trigrams (an AND, over-broad), then read each candidate's
/// stored text and check the substring is really there. Both counts and the
/// whole time, verification included, are what this returns.
fn trigram_candidates(field: tantivy::schema::Field, needle: &str) -> Box<dyn Query> {
    let lower = needle.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let terms: Vec<Box<dyn Query>> = chars
        .windows(3)
        .map(|w| Box::new(TermQuery::new(Term::from_field_text(field, &w.iter().collect::<String>()),
                                         IndexRecordOption::Basic)) as Box<dyn Query>)
        .collect();
    Box::new(BooleanQuery::intersection(terms))
}

/// (candidates, verified documents, occurrences in the first `span_limit`
/// verified documents, ms for all of it).
fn verified_substring(built: &Built, needle: &str, span_limit: usize) -> (usize, usize, usize, f64) {
    let reader = built.index.reader().unwrap();
    let searcher = reader.searcher();
    let lower = needle.to_lowercase();
    let t = Instant::now();
    let q = trigram_candidates(built.content, needle);
    let addrs = searcher.search(&*q, &DocSetCollector).unwrap();
    let candidates = addrs.len();
    let mut verified = 0;
    let mut spans = 0;
    for addr in addrs {
        let doc: TantivyDocument = searcher.doc(addr).unwrap();
        let text = doc.get_first(built.content).and_then(|v| v.as_value().as_str()).unwrap_or("");
        let hay = text.to_lowercase();
        if hay.contains(&lower) {
            verified += 1;
            if verified <= span_limit {
                spans += hay.matches(&lower).count();
            }
        }
    }
    (candidates, verified, spans, t.elapsed().as_secs_f64() * 1000.0)
}

fn count_and_time(built: &Built, query: &dyn Query) -> (usize, f64) {
    let reader = built.index.reader().unwrap();
    let searcher = reader.searcher();
    let t = Instant::now();
    let n = searcher.search(query, &Count).unwrap();
    (n, t.elapsed().as_secs_f64() * 1000.0)
}

/// The whole task: documents *and* where inside them the match is.
///
/// tantivy has `SnippetGenerator`, which re-reads the stored text and
/// re-analyses it per hit — the same shape of work Elasticsearch does for
/// highlighting, and the same reason it is charged here rather than left out.
fn count_with_spans(built: &Built, query: &dyn Query, limit: usize) -> (usize, usize, f64) {
    let reader = built.index.reader().unwrap();
    let searcher = reader.searcher();
    let t = Instant::now();
    let n = searcher.search(query, &Count).unwrap();
    let top = searcher.search(query, &TopDocs::with_limit(limit)).unwrap();
    let mut generator =
        tantivy::snippet::SnippetGenerator::create(&searcher, query, built.content).unwrap();
    generator.set_max_num_chars(usize::MAX / 4);
    let mut spans = 0;
    for (_score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr).unwrap();
        let snippet = generator.snippet_from_doc(&doc);
        spans += snippet.highlighted().len();
    }
    (n, spans, t.elapsed().as_secs_f64() * 1000.0)
}

#[test]
#[ignore]
fn compare_tantivy() {
    let root = corpus_root();
    eprintln!("=== corpus: {root} ===");
    let files = collect(Path::new(&root));
    let bytes: usize = files.iter().map(|(_, c)| c.len()).sum();
    eprintln!("{} files, {:.0} MB of text\n", files.len(), bytes as f64 / 1_048_576.0);
    assert!(!files.is_empty(), "empty corpus — set CMP_CORPUS");

    eprintln!("=== indexing: tantivy, default tokenizer ===");
    let plain = build(&files, "/tmp/tv_default", false);
    eprintln!("  {:.1}s, {:.0} MB\n", plain.seconds, plain.bytes as f64 / 1_048_576.0);

    eprintln!("=== indexing: tantivy, trigram tokenizer ===");
    let tri = build(&files, "/tmp/tv_ngram", true);
    eprintln!("  {:.1}s, {:.0} MB ({:.1}x the default index)\n",
              tri.seconds, tri.bytes as f64 / 1_048_576.0,
              tri.bytes as f64 / plain.bytes.max(1) as f64);

    let _parser_tri = QueryParser::for_index(&tri.index, vec![tri.content]);
    let parser_plain = QueryParser::for_index(&plain.index, vec![plain.content]);

    eprintln!("{:<40} {:>9} {:>10} {:>12}", "query", "hits", "time", "index");
    eprintln!("{}", "-".repeat(76));

    // Every row also goes to `CMP_OUT` as JSON (with the lucivy query it is
    // judged against, `V3_QUERIES` syntax), for the report that puts the
    // engines side by side (`benches/compare_engines.sh`).
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut row = |label: &str, truth: &str, hits: usize, ms: f64, which: &str, note: &str| {
        eprintln!("{label:<40} {hits:>9} {ms:>8.1}ms {which:>12}");
        if !note.is_empty() { eprintln!("{:<40} {note}", ""); }
        rows.push(serde_json::json!({"query": label, "truth": truth, "hits": hits, "ms": ms, "index": which, "note": note}));
    };

    // Substring, which needs the trigram index — and an explicit phrase, see
    // `trigram_phrase`.
    for needle in ["mutex_lock", "spin_lock", "sched"] {
        let q = trigram_phrase(tri.content, needle);
        let (n0, ms0) = count_and_time(&tri, &*q);
        let (cand, n, _spans, ms) = verified_substring(&tri, needle, 0);
        row(&format!("{needle} (substring)"), &format!("{needle}:strict"), n, ms, "trigram",
            &format!("trigram phrase: {n0} in {ms0:.1} ms (positions are all 0); AND of trigrams: {cand} candidates, {n} verified on the stored text"));
    }

    // Whole words, the default index.
    let q = parser_plain.parse_query("sched").unwrap();
    let (n, ms) = count_and_time(&plain, &*q);
    row("sched (whole word)", "sched:term", n, ms, "default", "");

    // Start of a token: a prefix over the default index's terms.
    let q = parser_plain.parse_query("printk*").unwrap();
    let (n, ms) = count_and_time(&plain, &*q);
    row("printk (start of token)", "printk:sw", n, ms, "default", "a prefix query over analysed terms");

    // Fuzzy: a Levenshtein automaton over *terms*. Nothing here can span a
    // separator, which is the point the panel is meant to establish.
    for (needle, d) in [("schdule", 1u8), ("regsiter", 2u8)] {
        let term = Term::from_field_text(plain.content, needle);
        let q = FuzzyTermQuery::new(term, d, true);
        let (n, ms) = count_and_time(&plain, &q);
        row(&format!("{needle} (fuzzy, {d} edit)"), &format!("{needle}:fz{d}"), n, ms, "default",
            "Levenshtein over whole terms, not across a separator");
    }

    // Regex, also over terms: `spin_lock_[a-z]+` cannot match, because the
    // default tokenizer already cut `spin`, `lock`, `irqsave` apart.
    match RegexQuery::from_pattern("spin_lock_[a-z]+", plain.content) {
        Ok(q) => {
            let (n, ms) = count_and_time(&plain, &q);
            row("spin_lock_[a-z]+ (regex, terms)", "spin_lock_[a-z]+:rx", n, ms, "default",
                "the default tokenizer cut spin, lock and irqsave apart: nothing to match");
        }
        Err(e) => eprintln!("regex on the default index: {e}"),
    }

    // ── Where the questions differ ──
    // Separators relaxed: `spin_lock` should also match `spin lock`, `spin-lock`,
    // `spinlock`. The trigram index cannot (its trigrams carry the underscore);
    // the default index can only be relaxed: the separator never enters it.
    let (cand, n, _s, ms) = verified_substring(&tri, "spinlock", 0);
    row("spinlock (trigrams, verified; must find spin_lock too)", "spin_lock:relax", n, ms, "trigram",
        &format!("inexpressible on trigrams, the underscore is in them: {cand} candidates, {n} verified hold the literal spinlock"));
    let q = parser_plain.parse_query("\"spin lock\"").unwrap();
    let (n, ms) = count_and_time(&plain, &*q);
    row("\"spin lock\" (phrase, default tokenizer)", "spin_lock:relax", n, ms, "default",
        "spin_lock, spin-lock and spin lock tokenise alike: relaxed is the only mode it has");
    let (cand, n, _s, ms) = verified_substring(&tri, "spin_lock", 0);
    row("spin_lock (trigrams, verified, strict)", "spin_lock:strict", n, ms, "trigram",
        &format!("{cand} candidates, {n} verified"));
    // Fuzzy across the boundary: `spinlokc` at two edits should reach `spin_lock`.
    let term = Term::from_field_text(plain.content, "spinlokc");
    let q = FuzzyTermQuery::new(term, 2, true);
    let (n, ms) = count_and_time(&plain, &q);
    row("spinlokc (fuzzy, 2 edits, across the boundary)", "spinlokc:fz2", n, ms, "default",
        "reaches the token spinlock, never spin_lock, already cut in two");
    // Short needles: below three characters there is no trigram to look up.
    let q = trigram_phrase(tri.content, "ude");
    let (n, ms) = count_and_time(&tri, &*q);
    row("ude (three characters)", "ude:strict", n, ms, "trigram", "");
    row("de (two characters)", "de:strict", 0, 0.0, "trigram",
        "no trigram exists for two characters: an n-gram index answers zero");

    // The same query, this time asked where it matched.
    eprintln!("\n{:<40} {:>9} {:>9} {:>12}", "documents AND spans", "docs", "spans", "time");
    eprintln!("{}", "-".repeat(76));
    // `SnippetGenerator` over a trigram phrase highlights nothing (the phrase
    // matches nothing); the application's path is the verification above, which
    // has the text in hand: count the occurrences in the first 200 verified docs.
    let (_cand, n, spans, ms) = verified_substring(&tri, "mutex_lock", 200);
    eprintln!("{:<40} {n:>9} {spans:>9} {ms:>10.1}ms", "mutex_lock, first 200 verified, occurrences");

    eprintln!("\nindexing: default {:.1}s / {:.0} MB — trigram {:.1}s / {:.0} MB",
              plain.seconds, plain.bytes as f64 / 1_048_576.0,
              tri.seconds, tri.bytes as f64 / 1_048_576.0);

    if let Ok(out) = std::env::var("CMP_OUT") {
        let report = serde_json::json!({
            "corpus": {"root": root, "files": files.len(), "bytes": bytes},
            "indexing": {
                "default": {"seconds": plain.seconds, "bytes": plain.bytes},
                "trigram": {"seconds": tri.seconds, "bytes": tri.bytes},
            },
            "queries": rows,
            "highlight": {"truth": "mutex_lock:strict", "docs": n, "highlighted": 200.min(n), "spans": spans, "ms": ms, "how": "AND of trigrams, then the stored text of every candidate read and searched; occurrences counted in the first 200 verified documents"},
        });
        std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        eprintln!("written to {out}");
    }
}
#[test]
#[ignore]
fn probe_ngram_positions() {
    use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer, TokenStream, Tokenizer};
    let mut a = TextAnalyzer::builder(NgramTokenizer::new(3, 3, false).unwrap())
        .filter(LowerCaser)
        .build();
    let mut st = a.token_stream("a spin_lock b");
    let mut seen = Vec::new();
    while st.advance() {
        let t = st.token();
        seen.push((t.position, t.text.clone(), t.offset_from, t.offset_to));
    }
    eprintln!("{} tokens pour \"a spin_lock b\"", seen.len());
    for (p, txt, f, to) in seen.iter().take(14) {
        eprintln!("  pos={p:<3} {txt:?} [{f}..{to}]");
    }
}

/// Can tantivy tell `spin_lock` from `spin-lock`?
///
/// Its default tokenizer decides. If it splits on the separator, the separator
/// is gone from the index and a phrase over `spin` + `lock` matches every
/// spelling — relaxed matching, with no way back to strict. If it keeps the
/// token whole, the opposite. Either way the answer is a property of the
/// tokenizer, so ask it rather than reason about it.
#[test]
#[ignore]
fn probe_default_tokenizer() {
    use tantivy::tokenizer::{SimpleTokenizer, TextAnalyzer, TokenStream, Tokenizer};
    let mut a = TextAnalyzer::builder(SimpleTokenizer::default()).build();
    for input in ["spin_lock", "spin-lock", "spin lock", "raw_spin_lock_irqsave"] {
        let mut st = a.token_stream(input);
        let mut toks = Vec::new();
        while st.advance() {
            let t = st.token();
            toks.push(format!("{}@{}", t.text, t.position));
        }
        eprintln!("  {input:<24} -> {}", toks.join(" "));
    }
}
