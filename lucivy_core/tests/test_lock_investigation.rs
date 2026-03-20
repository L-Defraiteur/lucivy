//! Check if "audioclock" has the suffix "lock" in the FST

#[test]
fn check_audioclock_suffix() {
    let shard_dir = "/home/luciedefraiteur/lucivy_bench_sharding/token_aware/shard_2";
    if !std::path::Path::new(shard_dir).exists() {
        eprintln!("Skipping"); return;
    }

    let dir = lucivy_core::directory::StdFsDirectory::open(shard_dir).unwrap();
    let handle = lucivy_core::handle::LucivyHandle::open(dir).unwrap();
    let field = handle.field("content").unwrap();
    let searcher = handle.reader.searcher();

    for seg_reader in searcher.segment_readers() {
        let sfx_slice = seg_reader.sfx_file(field).unwrap();
        let sfx_bytes = sfx_slice.read_bytes().unwrap();
        let sfx_reader = ld_lucivy::suffix_fst::file::SfxFileReader::open(sfx_bytes.as_ref()).unwrap();

        // Check all suffixes that resolve to ordinal 76900 (audioclock)
        let walk_all = sfx_reader.prefix_walk(""); // walk ALL entries
        let mut found_76900 = Vec::new();
        for (key, parents) in &walk_all {
            for p in parents {
                if p.raw_ordinal == 76900 {
                    found_76900.push((key.clone(), p.si));
                }
            }
        }
        eprintln!("Entries with raw_ordinal=76900: {}", found_76900.len());
        for (key, si) in &found_76900 {
            eprintln!("  suffix={:?} si={}", key, si);
        }

        // Also: what does prefix_walk("lock") return for SI_REST entries?
        let walk_lock = sfx_reader.prefix_walk("lock");
        eprintln!("\nprefix_walk('lock'): {} entries", walk_lock.len());

        // Specifically check: is "lock" at SI=6 (from "audioclock") in the walk?
        let has_audioclock = walk_lock.iter()
            .flat_map(|(_, parents)| parents)
            .any(|p| p.raw_ordinal == 76900);
        eprintln!("audioclock (ord=76900) in walk: {}", has_audioclock);

        // Check resolve_suffix for "lock" (all SIs)
        let resolved = sfx_reader.resolve_suffix("lock");
        eprintln!("\nresolve_suffix('lock'): {} parents", resolved.len());
        let has_76900 = resolved.iter().any(|p| p.raw_ordinal == 76900);
        eprintln!("audioclock in resolve_suffix: {}", has_76900);

        break;
    }
}
