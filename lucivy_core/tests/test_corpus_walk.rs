//! The corpus walk of the ground-truth tests (`common::corpus::walk`) says what it leaves out, in an order
//! that does not depend on the file system, and never reads `.vault/` or `.env*` (tickets of 18 November 2026,
//! « le parcours de la vérité écarte des fichiers sans le dire » and « des tests de lucivy lisent des chemins
//! personnels et passent en sautant »).

mod common;

use std::path::Path;

use common::corpus::{walk, Rules};

fn rules() -> Rules<'static> {
    Rules {
        exclude_dirs: &["target"],
        extensions: None,
        max_size: 100_000,
        skip_zero_size: true,
        skip_binary: true,
        skip_blank: true,
        max_files: usize::MAX,
        relative: true,
    }
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn the_walk_says_what_it_leaves_out_and_reads_no_secret() {
    let scratch = common::scratch("corpus-walk");
    let root = scratch.path();
    // Written in an order the sorted walk does not follow.
    write(root, "zeta.txt", b"last by name");
    write(root, "alpha.txt", b"first by name");
    write(root, "middle/beta.txt", b"in a directory");
    write(root, "not-utf8.txt", &[0x66, 0x6f, 0xff, 0xfe]);
    write(root, ".vault/token.txt", b"a secret the walk must not read");
    write(root, ".env.local", b"KEY=another secret");
    write(root, "locked/hidden.txt", b"behind a directory the walk cannot read");
    let locked = root.join("locked");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Under root a directory without permissions is still readable: that case cannot be made here.
    let locked_is_unreadable = std::fs::read_dir(&locked).is_err();

    let first = walk(root, &rules());
    let second = walk(root, &rules());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    let names: Vec<&str> = first.files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["alpha.txt", "middle/beta.txt", "zeta.txt"], "sorted, and no secret read");
    assert_eq!(first.files, second.files, "two walks of the same tree, the same list");
    assert_eq!(first.skipped.get("not UTF-8"), Some(&1), "the file that is not UTF-8 is counted: {:?}", first.skipped);
    assert_eq!(first.skipped.get("secret path"), Some(&2), ".vault/ and .env.local are counted: {:?}", first.skipped);
    if locked_is_unreadable {
        assert_eq!(first.skipped.get("unreadable directory"), Some(&1), "the unreadable directory is counted: {:?}", first.skipped);
    } else {
        println!("running as root: the unreadable directory cannot be made, that case is not checked");
    }
}
