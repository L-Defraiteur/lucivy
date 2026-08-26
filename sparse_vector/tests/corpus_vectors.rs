//! Sparse vectors with the shape of real ones, from real text.
//!
//! The benches used to spread dimensions by hashing and give every weight
//! 1.0. That is the one distribution WAND cannot be measured on: its pruning
//! comes entirely from the **imbalance** — a few dimensions with enormous
//! posting lists and low weights, a long tail with short lists and high ones.
//! Flat dimensions and equal weights make the score bound flat, and the
//! pivot then behaves like nothing real.
//!
//! So the vectors here come from text: the repository's own files (or
//! `$V3_CORPUS`), one word one dimension, hashed to a `u32` token id, and
//! weighted `tf · idf` — Zipf comes for free, because that is how words are
//! distributed. It is not SPLADE, and the weights are not a model's; the
//! *shape* is real, which is what the measurement needs.
//!
//! The honest fixture is still a dump of real BGE-M3 vectors, asked for from
//! the rag3weaver session; this generator will be recalibrated on it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sparse_vector::index::SparseVector;

/// A corpus of documents as sparse vectors, plus queries drawn from it.
pub struct Corpus {
    pub docs: Vec<(u64, SparseVector)>,
    pub queries: Vec<SparseVector>,
    /// Distinct dimensions across the corpus.
    pub dims: usize,
}

/// FNV-1a: a word to a dimension, stable across runs and machines.
fn token_id(word: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in word.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Text files under `root`, up to `max` of them, biggest directories first
/// in walk order (deterministic: entries are sorted).
fn text_files(root: &Path, max: usize) -> Vec<PathBuf> {
    const EXTENSIONS: [&str; 10] = ["rs", "md", "toml", "js", "mjs", "py", "html", "json", "txt", "yml"];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if out.len() >= max {
                return out;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| EXTENSIONS.contains(&&*e.to_string_lossy())) {
                out.push(path);
            }
        }
    }
    out
}

/// Split on anything that is not a letter or a digit, lowercased, words of
/// two characters or more.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_lowercase())
}

/// Where the text comes from: `$V3_CORPUS`, else the repository this test
/// belongs to.
pub fn corpus_root() -> PathBuf {
    if let Ok(dir) = std::env::var("V3_CORPUS") {
        return PathBuf::from(dir);
    }
    // `sparse_vector/tests/` → the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `n` documents, each a chunk of real text as a `tf · idf` sparse vector,
/// and `queries` query vectors: the rarest words of a document, which is
/// what a query encoder produces — few dimensions, high weights.
///
/// Chunks rather than whole files, so `nnz` per document stays in the range
/// a model produces (a 100 kB file would be one vector of 5 000 dimensions).
pub fn build(n: usize, queries: usize) -> Corpus {
    let root = corpus_root();
    let files = text_files(&root, 4_000);
    assert!(!files.is_empty(), "no text found under {}", root.display());

    // Pass 1: chunks of ~120 words, as term frequencies.
    let mut raw: Vec<HashMap<u32, f32>> = Vec::with_capacity(n);
    let mut document_freq: HashMap<u32, u32> = HashMap::new();
    'files: for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let mut chunk: HashMap<u32, f32> = HashMap::new();
        let mut count = 0usize;
        for word in words(&text) {
            *chunk.entry(token_id(&word)).or_insert(0.0) += 1.0;
            count += 1;
            if count == 120 {
                for &dim in chunk.keys() {
                    *document_freq.entry(dim).or_insert(0) += 1;
                }
                raw.push(std::mem::take(&mut chunk));
                count = 0;
                if raw.len() >= n {
                    break 'files;
                }
            }
        }
    }
    // Whatever the corpus holds: the repository yields about 49 000 chunks,
    // a bigger `$V3_CORPUS` yields more. A bench prints what it got.
    assert!(!raw.is_empty(), "no text under {}", root.display());

    // Pass 2: tf · idf, which is where the imbalance lives — a word in every
    // chunk weighs nothing, a rare one weighs a lot.
    let total = raw.len() as f32;
    let idf = |dim: u32| {
        let df = *document_freq.get(&dim).unwrap_or(&1) as f32;
        (1.0 + (total / df).ln()).max(0.05)
    };
    let docs: Vec<(u64, SparseVector)> = raw.iter().enumerate().map(|(i, chunk)| {
        let mut dims: Vec<(u32, f32)> = chunk.iter()
            .map(|(&dim, &tf)| (dim, (1.0 + tf.ln()) * idf(dim)))
            .collect();
        dims.sort_by_key(|(dim, _)| *dim);
        let indices: Vec<u32> = dims.iter().map(|(d, _)| *d).collect();
        let values: Vec<f32> = dims.iter().map(|(_, w)| *w).collect();
        (i as u64, SparseVector { indices, values })
    }).collect();

    // Queries: twenty-odd dimensions of a document, taken **across** its
    // weight range rather than off the top. A query of rare words alone hits
    // almost nothing and every search looks instant; a model's query vector
    // carries common terms too, and those are the long posting lists WAND
    // has to prune. One in three dimensions comes from the heavy end, the
    // rest are spread over the whole document.
    let queries: Vec<SparseVector> = (0..queries).map(|q| {
        let doc = &docs[(q * 7 + 3) % docs.len()].1;
        let mut ranked: Vec<(u32, f32)> = doc.indices.iter().copied()
            .zip(doc.values.iter().copied())
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let want = 24.min(ranked.len());
        let heavy = want / 3;
        let mut dims: Vec<(u32, f32)> = ranked[..heavy.min(ranked.len())].to_vec();
        let rest = &ranked[heavy.min(ranked.len())..];
        if !rest.is_empty() {
            let step = (rest.len() / (want - heavy).max(1)).max(1);
            dims.extend(rest.iter().step_by(step).take(want - heavy).copied());
        }
        dims.sort_by_key(|(dim, _)| *dim);
        dims.dedup_by_key(|(dim, _)| *dim);
        SparseVector {
            indices: dims.iter().map(|(d, _)| *d).collect(),
            values: dims.iter().map(|(_, w)| *w).collect(),
        }
    }).collect();

    let mut all: Vec<u32> = docs.iter().flat_map(|(_, v)| v.indices.iter().copied()).collect();
    all.sort_unstable();
    all.dedup();
    Corpus { dims: all.len(), docs, queries }
}

/// What the corpus looks like, for a bench to print: the imbalance is the
/// point, so it is worth showing.
pub fn describe(corpus: &Corpus) -> String {
    let mut nnz: Vec<usize> = corpus.docs.iter().map(|(_, v)| v.indices.len()).collect();
    nnz.sort_unstable();
    let mut df: HashMap<u32, usize> = HashMap::new();
    for (_, v) in &corpus.docs {
        for &d in &v.indices { *df.entry(d).or_insert(0) += 1; }
    }
    let mut lists: Vec<usize> = df.values().copied().collect();
    lists.sort_unstable();
    let median = |v: &[usize]| v[v.len() / 2];
    format!(
        "{} documents, {} dimensions — nnz per document {}/{}/{} (min/median/max), \
         posting lists {}/{}/{}, {} dimensions appear once",
        corpus.docs.len(), corpus.dims,
        nnz[0], median(&nnz), nnz[nnz.len() - 1],
        lists[0], median(&lists), lists[lists.len() - 1],
        lists.iter().filter(|&&n| n == 1).count(),
    )
}
