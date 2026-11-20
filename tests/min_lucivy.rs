//! **An index that a newer lucivy marked is refused by name** (20 November 2026, 4.3.13). lucivy 4.4 keeps tagged
//! generations on disk, files an older garbage collector would take for garbage; it writes `min_lucivy` in the
//! index settings, and an older lucivy that opens the index must refuse, saying which version it needs, before any
//! writer or collection runs.

use ld_lucivy::directory::RamDirectory;
use ld_lucivy::schema::{Schema, TEXT};
use ld_lucivy::{Index, IndexSettings};

fn index_needing(version: &str) -> RamDirectory {
    let mut schema_builder = Schema::builder();
    schema_builder.add_text_field("text", TEXT);
    let dir = RamDirectory::create();
    let settings = IndexSettings { min_lucivy: Some(version.to_string()), ..Default::default() };
    Index::builder().schema(schema_builder.build()).settings(settings).open_or_create(dir.clone()).unwrap();
    dir
}

#[test]
fn an_index_needing_a_newer_lucivy_is_refused_by_name() {
    let dir = index_needing("9.0.0");
    let err = Index::open(dir).err().expect("an index that needs lucivy 9.0.0 does not open");
    let said = err.to_string();
    assert!(said.contains("9.0.0") && said.contains(env!("CARGO_PKG_VERSION")), "the refusal names both versions: {said}");
}

#[test]
fn an_index_needing_this_lucivy_or_an_older_one_opens() {
    for v in ["4.3.0", env!("CARGO_PKG_VERSION")] {
        assert!(Index::open(index_needing(v)).is_ok(), "an index needing {v} opens");
    }
}
