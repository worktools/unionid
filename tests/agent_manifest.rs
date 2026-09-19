use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn agent_manifest_is_machine_readable_and_covers_the_error_contract() {
    let output = run(&["agent", "--format", "json"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["software_version"], env!("CARGO_PKG_VERSION"));

    let codes = value["error_contract"]["codes"].as_array().unwrap();
    assert!(codes.iter().any(|code| code == "E_CONSTRAINT"));
    assert!(codes.iter().any(|code| code == "E_PAGE_ORDER"));
    assert!(codes.iter().any(|code| code == "E_TABLE"));

    let kinds = value["error_contract"]["constraint_kinds"]
        .as_array()
        .unwrap();
    assert!(kinds.iter().any(|kind| {
        kind["kind"] == "partial_unique" && kind["hint"].as_str().is_some_and(|h| !h.is_empty())
    }));
    assert!(
        kinds
            .iter()
            .any(|kind| { kind["kind"] == "primary_key_missing" && kind["hint"].is_null() })
    );

    let commands = value["commands"].as_array().unwrap();
    assert!(commands.iter().any(|command| command["name"] == "run"));
    assert!(
        commands
            .iter()
            .any(|command| command["name"] == "agent" && command["json"] == true)
    );
    assert_eq!(value["current_storage"]["format"], 10);
}

#[test]
fn agent_manifest_markdown_lists_commands_and_hints() {
    let output = run(&["agent"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let markdown = String::from_utf8(output.stdout).unwrap();
    assert!(markdown.starts_with("# unionid agent manifest"));
    assert!(markdown.contains("## Commands"));
    assert!(markdown.contains("## Error contract"));
    assert!(markdown.contains("storage_format: 10"));
    assert!(markdown.contains("partial_unique"));
    assert!(markdown.contains("E_TABLE"));
    assert!(markdown.contains("E_CONSTRAINT"));
    assert!(markdown.contains("## Error codes"));
}
