mod common;

use common::TempDir;
use std::fs;
use unionid::migration::query_validation::{
    MAX_QUERY_DIRECTORY_DEPTH, MAX_QUERY_DIRECTORY_ENTRIES, MAX_QUERY_FILES, load_query_directory,
};

#[test]
fn nested_sources_keep_paths_and_are_loaded_once_in_sorted_order() {
    let dir = TempDir::new();
    fs::create_dir(dir.0.join("nested")).unwrap();
    fs::write(dir.0.join("nested/check.unid"), "from jobs").unwrap();
    fs::write(dir.0.join("check.uid"), "from other").unwrap();
    let queries = load_query_directory(&dir.0).unwrap();
    assert_eq!(
        queries.iter().map(|q| q.path.as_str()).collect::<Vec<_>>(),
        ["check.uid", "nested/check.unid"]
    );
    fs::write(dir.0.join("check.uid"), "changed").unwrap();
    assert_eq!(queries[0].source, "from other");
    assert_eq!(queries[1].source, "from jobs");
}

#[test]
fn rejects_missing_empty_foreign_and_non_utf8_sources() {
    let dir = TempDir::new();
    assert_eq!(
        load_query_directory(dir.0.join("missing"))
            .unwrap_err()
            .code,
        "E_IO"
    );
    assert_eq!(
        load_query_directory(&dir.0).unwrap_err().code,
        "E_MIGRATION"
    );
    fs::write(dir.0.join("README.md"), "not a query").unwrap();
    assert_eq!(
        load_query_directory(&dir.0).unwrap_err().code,
        "E_MIGRATION"
    );
    fs::remove_file(dir.0.join("README.md")).unwrap();
    fs::write(dir.0.join("bad.unid"), [0xff]).unwrap();
    assert_eq!(load_query_directory(&dir.0).unwrap_err().code, "E_INPUT");
}

#[test]
fn source_and_aggregate_byte_budgets_are_checked_before_binding() {
    let dir = TempDir::new();
    let source = " ".repeat(unionid::syntax::MAX_SOURCE_BYTES);
    fs::write(dir.0.join("a.unid"), format!("{source} ")).unwrap();
    assert_eq!(load_query_directory(&dir.0).unwrap_err().code, "E_LIMIT");
    fs::write(dir.0.join("a.unid"), &source).unwrap();
    assert!(load_query_directory(&dir.0).is_ok());
    for n in 0..16 {
        fs::write(dir.0.join(format!("{n}.unid")), &source).unwrap();
    }
    assert_eq!(load_query_directory(&dir.0).unwrap_err().code, "E_LIMIT");
}

#[test]
fn file_depth_and_empty_directory_budgets_are_bounded() {
    let files = TempDir::new();
    for n in 0..=MAX_QUERY_FILES {
        fs::write(files.0.join(format!("{n}.unid")), "from jobs").unwrap();
    }
    assert_eq!(load_query_directory(&files.0).unwrap_err().code, "E_LIMIT");
    let deep = TempDir::new();
    let mut path = deep.0.clone();
    for _ in 0..=MAX_QUERY_DIRECTORY_DEPTH {
        path.push("d");
        fs::create_dir(&path).unwrap();
    }
    assert_eq!(load_query_directory(&deep.0).unwrap_err().code, "E_LIMIT");
    let wide = TempDir::new();
    for n in 0..=MAX_QUERY_DIRECTORY_ENTRIES {
        fs::create_dir(wide.0.join(n.to_string())).unwrap();
    }
    assert_eq!(load_query_directory(&wide.0).unwrap_err().code, "E_LIMIT");
}

#[cfg(unix)]
#[test]
fn rejects_root_and_nested_symlinks() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new();
    let root = dir.0.join("queries");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.unid"), "from jobs").unwrap();
    symlink(&root, dir.0.join("link")).unwrap();
    assert_eq!(
        load_query_directory(dir.0.join("link")).unwrap_err().code,
        "E_MIGRATION"
    );
    symlink(root.join("a.unid"), root.join("b.unid")).unwrap();
    assert_eq!(load_query_directory(&root).unwrap_err().code, "E_MIGRATION");
    fs::remove_file(root.join("b.unid")).unwrap();
    symlink(&root, root.join("loop")).unwrap();
    assert_eq!(load_query_directory(&root).unwrap_err().code, "E_MIGRATION");
    fs::remove_file(root.join("loop")).unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        fs::write(
            root.join(std::ffi::OsString::from_vec(b"\xff.unid".to_vec())),
            "from jobs",
        )
        .unwrap();
        assert_eq!(load_query_directory(&root).unwrap_err().code, "E_MIGRATION");
    }
}
