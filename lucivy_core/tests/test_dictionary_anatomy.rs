//! What a shard dictionary holds, read from its files without opening the
//! index: per field, the chunk entries (8 bytes at most) and the
//! word-stripped entries (whole words, up to `WORD_SUFFIX_CAP` suffixes
//! each), their text bytes, a histogram of word lengths with the suffixes
//! each band costs, and the `.sfx` / `.termtexts` / `.pidx` bytes — so that
//! "why is the dictionary bigger than the text" has an answer in numbers.
//!
//!     LUCIVY_ANATOMY_INDEX=<index directory: shard_*/dict-*> \
//!     cargo test --release -p lucivy-core --test test_dictionary_anatomy -- --ignored --nocapture

use std::collections::BTreeMap;

use ld_lucivy::suffix_fst::termtexts_v3::{TermTextsReaderV3, WORD_SUFFIX_CAP};

const BANDS: &[(usize, usize)] = &[(1, 8), (9, 16), (17, 32), (33, 64), (65, 128), (129, 256), (257, usize::MAX)];

#[derive(Default)]
struct Field {
    chunks: u64,
    chunk_bytes: u64,
    words: u64,
    word_bytes: u64,
    /// Per band: (words, text bytes, suffixes).
    bands: Vec<(u64, u64, u64)>,
    longest: Vec<(usize, String)>,
    files: BTreeMap<&'static str, u64>,
}

#[test]
#[ignore]
fn dictionary_anatomy() {
    let root = std::env::var("LUCIVY_ANATOMY_INDEX").expect("LUCIVY_ANATOMY_INDEX");
    let mut fields: BTreeMap<String, Field> = BTreeMap::new();
    let mut shards: Vec<_> = std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("shard_")).map(|e| e.path()).collect();
    shards.sort();
    for shard in &shards {
        for entry in std::fs::read_dir(shard).unwrap().filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix("dict-") else { continue };
            let mut parts = rest.splitn(3, '.');
            let (_gen, field, ext) = (parts.next().unwrap(), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let f = fields.entry(field.to_string()).or_default();
            if f.bands.is_empty() { f.bands = vec![(0, 0, 0); BANDS.len()]; }
            let size = entry.metadata().unwrap().len();
            let kind: &'static str = match ext { "sfx" => "sfx", "termtexts" => "termtexts", "pidx" => "pidx", _ => "other" };
            *f.files.entry(kind).or_default() += size;
            if ext != "termtexts" { continue }
            let bytes = std::fs::read(entry.path()).unwrap();
            let reader = TermTextsReaderV3::open(&bytes).expect("termtexts");
            for (_id, text, meta) in reader.iter() {
                let len = text.len();
                if meta.is_word_stripped {
                    f.words += 1;
                    f.word_bytes += len as u64;
                    let band = BANDS.iter().position(|&(lo, hi)| len >= lo && len <= hi).unwrap();
                    f.bands[band].0 += 1;
                    f.bands[band].1 += len as u64;
                    f.bands[band].2 += len.min(WORD_SUFFIX_CAP as usize) as u64;
                    if len > 64 {
                        f.longest.push((len, text.chars().take(60).collect()));
                    }
                } else {
                    f.chunks += 1;
                    f.chunk_bytes += len as u64;
                }
            }
        }
    }
    let mb = |b: u64| b as f64 / 1e6;
    for (field, f) in &mut fields {
        println!("\n== field {field}: files {:?} (Mo)", f.files.iter().map(|(k, v)| (*k, (mb(*v) * 10.0).round() / 10.0)).collect::<Vec<_>>());
        println!("chunks: {} entries, {:.1} Mo of text ; words: {} entries, {:.1} Mo of text", f.chunks, mb(f.chunk_bytes), f.words, mb(f.word_bytes));
        println!("word length\twords\ttext Mo\tsuffixes (M)\tshare of word suffixes");
        let total: u64 = f.bands.iter().map(|b| b.2).sum::<u64>().max(1);
        for (i, &(lo, hi)) in BANDS.iter().enumerate() {
            let (n, bytes, suffixes) = f.bands[i];
            let label = if hi == usize::MAX { format!("{lo}+") } else { format!("{lo}-{hi}") };
            println!("{label}\t{n}\t{:.1}\t{:.2}\t{:.1} %", mb(bytes), suffixes as f64 / 1e6, 100.0 * suffixes as f64 / total as f64);
        }
        f.longest.sort_by(|a, b| b.0.cmp(&a.0));
        for (len, start) in f.longest.iter().take(8) {
            println!("  {len} bytes: {start:?}");
        }
    }
}
