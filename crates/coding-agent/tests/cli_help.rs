use std::process::Command;

#[test]
fn help_lists_thinking_possible_values() {
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .arg("--help")
        .output()
        .expect("run pie --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[possible values: off, minimal, low, medium, high, xhigh]"),
        "help should list accepted --thinking values:\n{stdout}"
    );
}

#[test]
fn help_lists_model_catalog_entry_points() {
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .arg("--help")
        .output()
        .expect("run pie --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Model catalog:"), "{stdout}");
    assert!(stdout.contains("Supported providers"), "{stdout}");
    assert!(stdout.contains("anthropic("), "{stdout}");
    assert!(stdout.contains("openai("), "{stdout}");
    assert!(stdout.contains("~/.pie/models.json"), "{stdout}");
    assert!(stdout.contains("<cwd>/.pie/models.json"), "{stdout}");
    assert!(
        stdout.contains("/model list") || stdout.contains("model list"),
        "{stdout}"
    );
    assert!(!stdout.contains("auth.json"), "{stdout}");
    assert!(!stdout.contains("API_KEY"), "{stdout}");
}

#[test]
fn invalid_thinking_value_reports_candidates() {
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(["--thinking", "turbo", "--list-sessions"])
        .output()
        .expect("run pie with invalid thinking value");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid value 'turbo'"), "{stderr}");
    assert!(
        stderr.contains("[possible values: off, minimal, low, medium, high, xhigh]"),
        "{stderr}"
    );
}
