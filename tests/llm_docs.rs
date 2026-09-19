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

#[test]
fn cli_lists_and_reads_categorized_bundled_docs() {
    let default_list = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .arg("docs")
        .output()
        .unwrap();
    assert!(default_list.status.success());
    let default_list = String::from_utf8(default_list.stdout).unwrap();
    assert!(default_list.contains("learn\n"));
    assert!(default_list.contains("language\n"));
    assert!(default_list.contains("operations\n"));
    assert!(default_list.contains("getting-started"));
    assert!(default_list.contains("observability"));
    assert!(default_list.contains("unionid docs show <topic>"));

    let language = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["docs", "list", "--category", "language", "--format", "json"])
        .output()
        .unwrap();
    assert!(language.status.success());
    let language: serde_json::Value = serde_json::from_slice(&language.stdout).unwrap();
    assert_eq!(language["schema_version"], 1);
    assert_eq!(language["topics"].as_array().unwrap().len(), 6);
    assert!(
        language["topics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|topic| topic["category"] == "language")
    );

    let query = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["docs", "show", "query"])
        .output()
        .unwrap();
    assert!(query.status.success());
    let query = String::from_utf8(query.stdout).unwrap();
    assert!(query.contains("topic: query\ncategory: language"));
    assert!(query.contains("# 查询语言参考"));

    let unknown = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["docs", "show", "unknown"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8(unknown.stderr)
            .unwrap()
            .contains("unionid docs list")
    );
}
