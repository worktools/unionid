use std::process::Command;

#[test]
fn cli_returns_versioned_llm_query_docs_as_markdown_and_json() {
    let markdown = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["docs", "query"])
        .output()
        .unwrap();
    assert!(markdown.status.success());
    assert!(markdown.stderr.is_empty());
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    assert!(markdown.starts_with("---\ndocument_schema_version: 1\n"));
    assert!(markdown.contains("# Unionid query language for LLMs"));
    assert!(markdown.contains("State::Pending"));
    assert!(markdown.contains("Pending =>"));
    assert!(markdown.contains("(value: Type) -> expression"));
    assert!(markdown.contains("union {"));
    assert!(markdown.contains("# Runnable examples"));

    let json = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["docs", "query", "--format", "json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    assert!(json.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["software_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["language_version"], "0.7");
    assert_eq!(json["topic"], "query");
    assert_eq!(json["examples"].as_array().unwrap().len(), 4);
    for example in json["examples"].as_array().unwrap() {
        let source = example["source"].as_str().unwrap();
        assert_eq!(unionid::format_source(source).unwrap(), source);
    }
}
