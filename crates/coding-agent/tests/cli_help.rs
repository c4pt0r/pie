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
