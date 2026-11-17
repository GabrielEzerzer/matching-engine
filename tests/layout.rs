//! The repository layout: every tracked file sits under a known top-level folder or is a known top-level file.

use std::process::Command;

const TOP_LEVEL_FOLDERS: [&str; 7] = [
    ".github", "examples", "harness", "scripts", "setup", "src", "tests",
];
const TOP_LEVEL_FILES: [&str; 11] = [
    ".gitattributes",
    ".gitignore",
    ".lint-baseline.json",
    "BENCHMARKING.md",
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "README.md",
    "features.json",
    "hush.toml",
];

fn misplaced<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    paths
        .into_iter()
        .filter(|path| match path.split_once('/') {
            Some((folder, _)) => !TOP_LEVEL_FOLDERS.contains(&folder),
            None => !TOP_LEVEL_FILES.contains(path),
        })
        .collect()
}

#[test]
fn every_tracked_file_sits_in_a_known_place() {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("git is on PATH");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed = String::from_utf8(out.stdout).expect("tracked paths are UTF-8");
    let wrong = misplaced(listed.split('\0').filter(|path| !path.is_empty()));
    assert!(
        wrong.is_empty(),
        "{wrong:?} sit outside the known layout. Move each one under a top-level folder in {TOP_LEVEL_FOLDERS:?}, \
         or, for a new top-level file, add it to the top-level files {TOP_LEVEL_FILES:?} in tests/layout.rs."
    );
}

#[test]
fn misplaced_names_files_outside_the_layout() {
    let paths = [
        "src/lib.rs",
        "README.md",
        "notes.txt",
        "docs/plan.md",
        "tests/layout.rs",
    ];
    assert_eq!(misplaced(paths), ["notes.txt", "docs/plan.md"]);
}
