use otto_tools::complete::{complete, score};

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in [
        (".gitignore", "target/\n*.log\n"),
        ("src/main.rs", ""),
        ("src/lib.rs", ""),
        ("deep/nested/main.rs", ""),
        ("docs/domain.md", ""),
        ("docs/guide.md", ""),
        ("guidelines/readme.md", ""),
        ("target/debug/out.rs", ""),
        ("run.log", ""),
        (".git/config", ""),
    ] {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    dir
}

fn paths(hits: &[(String, bool)]) -> Vec<&str> {
    hits.iter().map(|(p, _)| p.as_str()).collect()
}

#[test]
fn walks_the_project_without_ignored_files_or_git() {
    let dir = tree();
    let all = complete(dir.path(), "", 500);
    let names = paths(&all);
    for shown in [".gitignore", "src/", "src/main.rs", "docs/", "deep/nested/"] {
        assert!(names.contains(&shown), "{shown}: {names:?}");
    }
    assert!(names.iter().all(|p| !p.starts_with("target") && !p.ends_with(".log") && !p.starts_with(".git/")), "{names:?}");
    // Folders end in `/` and say so; shallow entries come first.
    assert!(all.iter().all(|(p, d)| p.ends_with('/') == *d));
    let at = |p: &str| names.iter().position(|n| *n == p).unwrap();
    assert!(at("src/") < at("src/main.rs"));
    assert_eq!(complete(dir.path(), "", 3).len(), 3);
}

#[test]
fn ranks_basename_matches_first() {
    let dir = tree();
    let hits = complete(dir.path(), "main", 50);
    let names = paths(&hits);
    let at = |p: &str| names.iter().position(|n| *n == p).unwrap_or_else(|| panic!("{p}: {names:?}"));
    assert_eq!(names[0], "src/main.rs");
    assert!(at("src/main.rs") < at("deep/nested/main.rs") && at("deep/nested/main.rs") < at("docs/domain.md"));
    let hits = complete(dir.path(), "guide", 50);
    let names = paths(&hits);
    let at = |p: &str| names.iter().position(|n| *n == p).unwrap();
    assert!(at("docs/guide.md") < at("guidelines/readme.md"));
    assert!(complete(dir.path(), "zzzq", 50).is_empty());
    // Case-insensitive, and a subsequence.
    assert_eq!(paths(&complete(dir.path(), "SLib", 50))[0], "src/lib.rs");
}

#[test]
fn a_folder_query_drills_into_it() {
    let dir = tree();
    let inside = complete(dir.path(), "src/", 50);
    assert_eq!(paths(&inside), ["src/lib.rs", "src/main.rs"]);
    assert_eq!(paths(&complete(dir.path(), "src/ma", 50)), ["src/main.rs"]);
    assert_eq!(paths(&complete(dir.path(), "deep/", 50))[0], "deep/nested/");
    // Not a folder: the whole query matches paths.
    assert!(paths(&complete(dir.path(), "nope/main", 50)).is_empty());
    assert!(complete(dir.path(), "../", 50).iter().all(|(p, _)| !p.starts_with("..")));
}

#[test]
fn score_prefers_word_starts_and_shallow_paths() {
    let needle: Vec<char> = "mr".chars().collect();
    assert!(score(&needle, "main.rs", 1) > score(&needle, "src/main.rs", 2));
    assert!(score(&needle, "src/main.rs", 2) > score(&needle, "src/hammer", 2));
    assert_eq!(score(&needle, "src/lib.rs", 2), None);
    assert!(score(&[], "a/", 1) > score(&[], "a/b", 2));
}
