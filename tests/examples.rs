use std::path::PathBuf;

use unionid::{Engine, format_source};

fn example_sources() -> Vec<(String, String)> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("unid") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).unwrap();
        sources.push((name, source));
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    assert!(!sources.is_empty(), "no top-level examples found");
    sources
}

#[test]
fn every_example_is_canonical_and_runs_in_memory() {
    for (name, source) in example_sources() {
        assert_eq!(
            format_source(&source).unwrap(),
            source,
            "{name} is not canonically formatted; run `unionid fmt`"
        );
        let mut engine = Engine::memory();
        let response = engine.execute(&source);
        assert!(response.ok, "{name} failed: {}", response.message);
    }
}
