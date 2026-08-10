use std::process::{Command, Output};

use computer_use_mcp::VERSION;

#[test]
fn cli_help_version_and_errors_are_truthful() {
    let help = run(&["help"]);
    assert!(help.status.success());
    let help_text = text(&help.stdout);
    assert!(help_text.contains("Computer Use MCP for Linux Wayland"));
    assert!(help_text.contains("init"));
    assert!(help_text.contains("call FILE"));
    let version = run(&["version"]);
    assert!(version.status.success());
    assert_eq!(text(&version.stdout).trim(), VERSION);

    let unknown = run(&["not-a-command"]);
    assert!(!unknown.status.success());
    assert!(text(&unknown.stderr).contains("unknown command"));

    let missing_file = run(&["call"]);
    assert!(!missing_file.status.success());
    assert!(text(&missing_file.stderr).contains("requires exactly one"));
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_computer-use-mcp"))
        .args(arguments)
        .output()
        .expect("run computer-use-mcp")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
