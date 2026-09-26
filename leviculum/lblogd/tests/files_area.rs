//! The file area against a real directory.
//!
//! The pure parts of `lblogd::files` (name sanitising, reference mapping, MIME
//! table) are unit-tested in the module itself. What needs a filesystem is the
//! load: which entries a directory yields, and which it deliberately drops.

use std::path::Path;

use lblogd::files::{load_files_dir, read_entry, FileArea};

/// A file area over a temp directory with a generous ceiling.
fn area(dir: &Path) -> FileArea {
    FileArea::new(dir)
}

#[test]
fn a_plain_directory_of_files_loads_by_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("antenne.png"), b"one").unwrap();
    std::fs::write(dir.path().join("mast.jpg"), b"twotwo").unwrap();

    let files = load_files_dir(&area(dir.path())).unwrap();
    let names: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(names, ["antenne.png", "mast.jpg"]);
    assert_eq!(files["antenne.png"].len, 3);
    assert_eq!(files["mast.jpg"].len, 6);
    assert_eq!(files["mast.jpg"].path, dir.path().join("mast.jpg"));
}

#[test]
fn a_missing_directory_yields_an_empty_area_rather_than_an_error() {
    // The file area is optional by existence: a blog with no pictures must
    // still start, and one that grows a directory later must start serving
    // from it on the next reload without a config change.
    let dir = tempfile::tempdir().unwrap();
    let files = load_files_dir(&area(&dir.path().join("nothing-here"))).unwrap();
    assert!(files.is_empty());
}

#[test]
fn a_file_above_the_ceiling_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("small.png"), vec![0u8; 64]).unwrap();
    std::fs::write(dir.path().join("huge.png"), vec![0u8; 4096]).unwrap();

    let files = load_files_dir(&FileArea {
        dir: dir.path().to_path_buf(),
        max_bytes: 1024,
    })
    .unwrap();
    let names: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["small.png"],
        "a file above max_file_bytes must not be served"
    );
}

#[test]
fn subdirectories_and_dotfiles_are_not_part_of_the_area() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested").join("deep.png"), b"x").unwrap();
    std::fs::write(dir.path().join(".hidden.png"), b"x").unwrap();
    std::fs::write(dir.path().join("visible.png"), b"x").unwrap();

    let files = load_files_dir(&area(dir.path())).unwrap();
    let names: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(names, ["visible.png"]);
}

#[test]
fn a_symlink_into_the_area_is_served() {
    // The area is operator-controlled: pointing it at photographs that live
    // elsewhere is a reasonable thing to want. The guard against a hostile
    // request is the name check, which no symlink helps a requester past.
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("original.png");
    std::fs::write(&target, b"pixels").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("antenne.png")).unwrap();

    let files = load_files_dir(&area(dir.path())).unwrap();
    assert_eq!(files["antenne.png"].len, 6);
    assert_eq!(read_entry(&files["antenne.png"], 1024).unwrap(), b"pixels");
}

#[test]
fn reading_an_entry_that_grew_past_the_ceiling_fails_rather_than_serving_it() {
    // The snapshot measured the file at load; it may have been rewritten
    // since, and serving from disk is what keeps large files out of memory.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.png"), vec![0u8; 16]).unwrap();
    let files = load_files_dir(&area(dir.path())).unwrap();
    let entry = &files["a.png"];

    std::fs::write(dir.path().join("a.png"), vec![0u8; 4096]).unwrap();
    let err = read_entry(entry, 1024).unwrap_err();
    assert!(err.to_string().contains("above max_file_bytes"), "{err}");
}
