//! **Measure, ignored: what the shard dictionary of an index holds** (19 November 2026). On the code base of
//! rag3weaver, the dictionary (`dict-*` files: the suffix FST and the term texts of every text field) weighed
//! 440 MB for 81 MB of stored text, half of the index. This walk says where the bytes go, per field: entries by
//! half (chunk or word-stripped), by trailing separator, by length and by kind of text; how many entries no
//! live segment names any more (ids kept after deletes and merges); how many distinct texts repeat across the
//! shards (each shard has its own dictionary). It reads only: point `LUCIVY_DICT_INDEX` at a **copy**.

use std::collections::{BTreeMap, HashMap, HashSet};

use ld_lucivy::suffix_fst::gmap::GmapReader;
use lucivy_core::sharded_handle::ShardedHandle;

#[derive(Default)]
struct Tally {
    entries: u64,
    bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: usize) {
        self.entries += 1;
        self.bytes += bytes as u64;
    }
}

fn kind(content: &str) -> &'static str {
    if content.is_empty() {
        "empty"
    } else if content.bytes().all(|b| b.is_ascii_digit()) {
        "digits"
    } else if content.len() >= 8 && content.bytes().all(|b| b.is_ascii_hexdigit()) && content.bytes().any(|b| b.is_ascii_digit()) {
        "hex ≥ 8"
    } else if !content.is_ascii() {
        "non-ASCII"
    } else if content.len() >= 32 {
        "ASCII ≥ 32 bytes"
    } else if content.bytes().all(|b| b.is_ascii_alphabetic()) {
        "letters"
    } else {
        "mixed ASCII"
    }
}

fn bucket(len: usize) -> &'static str {
    match len {
        0..=2 => "0-2",
        3..=4 => "3-4",
        5..=8 => "5-8",
        9..=16 => "9-16",
        17..=32 => "17-32",
        33..=64 => "33-64",
        _ => "65+",
    }
}

fn line(name: &str, t: &Tally, total: &Tally) {
    println!(
        "    {name:<24} {:>10} entries ({:>5.1} %)  {:>8.1} MB of text ({:>5.1} %)",
        t.entries,
        100.0 * t.entries as f64 / total.entries.max(1) as f64,
        t.bytes as f64 / 1e6,
        100.0 * t.bytes as f64 / total.bytes.max(1) as f64
    );
}

#[test]
#[ignore]
fn what_the_dictionary_holds() {
    let path = std::env::var("LUCIVY_DICT_INDEX").expect("LUCIVY_DICT_INDEX=<a copy of a sharded index>");
    let index = ShardedHandle::open(&path).unwrap();
    let text_fields: Vec<(String, ld_lucivy::schema::Field)> = index
        .field_map
        .iter()
        .filter(|(name, _)| !name.starts_with('_'))
        .filter(|(_, f)| index.shard(0).unwrap().index.sfx_dictionary().and_then(|d| d.field(f.field_id()).map(|_| ())).is_some())
        .cloned()
        .collect();
    // Bytes on disk of the dictionary files, per field id and extension.
    let mut on_disk: BTreeMap<(u32, String), u64> = BTreeMap::new();
    for entry in walkdir(&path) {
        let name = entry.file_name().unwrap().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("dict-") else { continue };
        let parts: Vec<&str> = rest.split('.').collect();
        if parts.len() == 3 {
            if let Ok(field) = parts[1].parse::<u32>() {
                *on_disk.entry((field, parts[2].to_string())).or_default() += std::fs::metadata(&entry).unwrap().len();
            }
        }
    }
    for (name, field) in &text_fields {
        let fid = field.field_id();
        println!("== field {name} (id {fid})");
        for ((f, ext), bytes) in &on_disk {
            if *f == fid {
                println!("  on disk .{ext}: {:.1} MB", *bytes as f64 / 1e6);
            }
        }
        let mut total = Tally::default();
        let mut by_half: BTreeMap<&str, Tally> = BTreeMap::new();
        let mut by_sep: BTreeMap<&str, Tally> = BTreeMap::new();
        let mut by_len: BTreeMap<&str, Tally> = BTreeMap::new();
        let mut by_kind: BTreeMap<&str, Tally> = BTreeMap::new();
        let mut unreferenced = Tally::default();
        // Distinct (half, lowercased content) → the shards that hold it.
        let mut shards_of: HashMap<(bool, String), u8> = HashMap::new();
        let mut distinct_per_shard = 0u64;
        let mut shard_id = 0;
        while let Some(shard) = index.shard(shard_id) {
            let dictionary = shard.index.sfx_dictionary().expect("an sfx_version 4 index");
            let dict_field = dictionary.field(fid).expect("the field's dictionary");
            let texts = dict_field.termtexts().expect("the term texts");
            let mut named: HashSet<u32> = HashSet::new();
            for segment in shard.reader.searcher().segment_readers() {
                if let Some(file) = segment.sfx_index_file("gmap", *field) {
                    let bytes = file.read_bytes().unwrap();
                    named.extend(GmapReader::open(bytes.as_slice()).expect("gmap").iter());
                }
            }
            let mut seen_here: HashSet<(bool, String)> = HashSet::new();
            for (id, text, meta) in texts.iter() {
                let own = (meta.own_len as usize).min(text.len());
                let content_len = own.saturating_sub(meta.sep_len as usize);
                let content = text.get(..content_len).unwrap_or(text);
                let size = text.len();
                total.add(size);
                by_half.entry(if meta.is_word_stripped { "word-stripped" } else { "chunk" }).or_default().add(size);
                by_sep
                    .entry(match (meta.sep_len, meta.overlap_len) {
                        (0, 0) => "no separator, no overlap",
                        (_, 0) => "with separator",
                        (0, _) => "with overlap",
                        _ => "separator and overlap",
                    })
                    .or_default()
                    .add(size);
                by_len.entry(bucket(content.len())).or_default().add(size);
                by_kind.entry(kind(content)).or_default().add(size);
                if !named.contains(&id) {
                    unreferenced.add(size);
                }
                let key = (meta.is_word_stripped, content.to_lowercase());
                if seen_here.insert(key.clone()) {
                    *shards_of.entry(key).or_default() += 1;
                }
            }
            distinct_per_shard += seen_here.len() as u64;
            println!(
                "  shard {shard_id}: {} entries, {} named by a live segment, {} distinct (half, lowercased content)",
                texts.num_terms(),
                named.len(),
                seen_here.len()
            );
            shard_id += 1;
        }
        println!("  entries: {} ({:.1} MB of term text)", total.entries, total.bytes as f64 / 1e6);
        println!("  by half:");
        for (k, t) in &by_half {
            line(k, t, &total);
        }
        println!("  by trailing bytes:");
        for (k, t) in &by_sep {
            line(k, t, &total);
        }
        println!("  by content length (bytes):");
        for (k, t) in &by_len {
            line(k, t, &total);
        }
        println!("  by kind of content:");
        for (k, t) in &by_kind {
            line(k, t, &total);
        }
        println!("  no live segment names them:");
        line("unreferenced", &unreferenced, &total);
        let union = shards_of.len() as u64;
        let mut in_n: BTreeMap<u8, u64> = BTreeMap::new();
        for n in shards_of.values() {
            *in_n.entry(*n).or_default() += 1;
        }
        println!(
            "  distinct (half, lowercased content): {union} across the shards, {distinct_per_shard} summed per shard (×{:.2}); \
             entries per distinct content: {:.2}",
            distinct_per_shard as f64 / union.max(1) as f64,
            total.entries as f64 / distinct_per_shard.max(1) as f64
        );
        for (n, count) in &in_n {
            println!("    in {n} shard(s): {count}");
        }
    }
}

fn walkdir(root: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// **Measure, ignored: what one entry per content would weigh** (19 November 2026, lever 1 of
/// `docs/lucivy/19-11-2026/05-ce-que-tient-le-dictionnaire.md` in rag3db). Nothing in the format changes: the
/// walk splits the seven entries per content into what case, the separator and the borrowed bytes add, then
/// builds beside the index, in memory, the `.sfx` and `.termtexts` of each shard's entries as they are (to check
/// the builder against the files on disk) and of one entry per (half, content) and per (half, lowercased
/// content). It also runs `check_dictionary` and counts the ids the live segments name that the texts lack.
#[test]
#[ignore]
fn what_one_entry_per_content_would_weigh() {
    use ld_lucivy::suffix_fst::dictionary_compact::generation_sfx_bytes;
    use ld_lucivy::suffix_fst::termtexts_v3::{TermMetaV3, TermTextsWriterV3};
    use std::time::Instant;

    let path = std::env::var("LUCIVY_DICT_INDEX").expect("LUCIVY_DICT_INDEX=<a copy of a sharded index>");
    let index = ShardedHandle::open(&path).unwrap();
    match index.check_dictionary() {
        Ok(check) => println!("check_dictionary: {check:?}"),
        Err(e) => println!("check_dictionary: error {e}"),
    }
    let build = |entries: &[(u32, String, TermMetaV3)]| -> (usize, usize, f64) {
        let t = Instant::now();
        let sfx = generation_sfx_bytes(entries).expect("the suffix FST");
        let mut texts = TermTextsWriterV3::new().with_ids(entries.iter().map(|e| e.0).collect());
        for (i, (_, text, m)) in entries.iter().enumerate() {
            texts.add(i as u32, text, *m);
        }
        let texts = texts.serialize();
        (sfx.len(), texts.len(), t.elapsed().as_secs_f64())
    };
    let fields: Vec<(String, ld_lucivy::schema::Field)> =
        index.field_map.iter().filter(|(name, _)| !name.starts_with('_')).cloned().collect();
    for (name, field) in &fields {
        let fid = field.field_id();
        let mut sums: BTreeMap<&str, (u64, u64, u64, f64)> = BTreeMap::new();
        let mut keys: BTreeMap<&str, u64> = BTreeMap::new();
        let mut shard_id = 0;
        let mut any = false;
        while let Some(shard) = index.shard(shard_id) {
            shard_id += 1;
            let Some(dictionary) = shard.index.sfx_dictionary() else { continue };
            let Some(dict_field) = dictionary.field(fid) else { continue };
            any = true;
            let texts = dict_field.termtexts().expect("the term texts");
            let mut named: HashSet<u32> = HashSet::new();
            for segment in shard.reader.searcher().segment_readers() {
                if let Some(file) = segment.sfx_index_file("gmap", *field) {
                    let bytes = file.read_bytes().unwrap();
                    named.extend(GmapReader::open(bytes.as_slice()).expect("gmap").iter());
                }
            }
            let mut entries: Vec<(u32, String, TermMetaV3)> = texts.iter().map(|(id, t, m)| (id, t.to_string(), m)).collect();
            entries.sort_by_key(|e| e.0);
            let held: HashSet<u32> = entries.iter().map(|e| e.0).collect();
            let missing = named.iter().filter(|id| !held.contains(id)).count();
            let max_held = entries.last().map(|e| e.0).unwrap_or(0);
            let above = named.iter().filter(|id| **id > max_held).count();
            println!("  {name} shard {}: {} ids named by live segments and absent from the texts ({} of them above the last id held, {max_held})",
                shard_id - 1, missing, above);
            // The four keys.
            let content_of = |t: &str, m: &TermMetaV3, with_sep: bool| -> String {
                let own = (m.own_len as usize).min(t.len());
                let end = if with_sep { own } else { own.saturating_sub(m.sep_len as usize) };
                t.get(..end).unwrap_or(t).to_string()
            };
            let mut k_text: HashSet<(bool, String)> = HashSet::new();
            let mut k_sep: HashSet<(bool, String)> = HashSet::new();
            let mut k_case: HashSet<(bool, String)> = HashSet::new();
            let mut k_lower: HashSet<(bool, String)> = HashSet::new();
            for (_, t, m) in &entries {
                k_text.insert((m.is_word_stripped, t.clone()));
                k_sep.insert((m.is_word_stripped, content_of(t, m, true)));
                k_case.insert((m.is_word_stripped, content_of(t, m, false)));
                k_lower.insert((m.is_word_stripped, content_of(t, m, false).to_lowercase()));
            }
            *keys.entry("1 entries").or_default() += entries.len() as u64;
            *keys.entry("2 distinct extended texts").or_default() += k_text.len() as u64;
            *keys.entry("3 content + separator").or_default() += k_sep.len() as u64;
            *keys.entry("4 content, case kept").or_default() += k_case.len() as u64;
            *keys.entry("5 content, lowercased").or_default() += k_lower.len() as u64;
            // The builds.
            let one_per = |set: &HashSet<(bool, String)>| -> Vec<(u32, String, TermMetaV3)> {
                let mut v: Vec<&(bool, String)> = set.iter().collect();
                v.sort();
                v.into_iter()
                    .enumerate()
                    .map(|(i, (stripped, c))| {
                        let len = c.len().min(u16::MAX as usize) as u16;
                        (i as u32, c.clone(), TermMetaV3 { own_len: len, sep_len: 0, overlap_len: 0, is_word_start: true, is_word_stripped: *stripped })
                    })
                    .collect()
            };
            for (label, list) in [
                ("as they are", entries.clone()),
                ("one per content, case kept", one_per(&k_case)),
                ("one per lowercased content", one_per(&k_lower)),
            ] {
                let (sfx, tt, secs) = build(&list);
                let s = sums.entry(label).or_default();
                s.0 += list.len() as u64;
                s.1 += sfx as u64;
                s.2 += tt as u64;
                s.3 += secs;
            }
        }
        if !any {
            continue;
        }
        println!("== field {name} (id {fid})");
        let entries = keys["1 entries"];
        for (k, n) in &keys {
            println!("  {k:<28} {n:>10}  (entries ÷ {:.2})", entries as f64 / (*n).max(1) as f64);
        }
        for (label, (n, sfx, tt, secs)) in &sums {
            println!(
                "  built {label:<28} {n:>10} entries: .sfx {:>7.1} MB, .termtexts {:>6.1} MB, {:>6.1} s",
                *sfx as f64 / 1e6,
                *tt as f64 / 1e6,
                secs
            );
        }
    }
}
