use std::io::Write;
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

#[test]
fn idle_mcp_never_initializes_a_desktop_and_background_worker_requires_proof() {
    let idle = run(&["mcp"]);
    assert!(idle.status.success(), "{}", text(&idle.stderr));
    assert!(idle.stdout.is_empty());
    assert!(!text(&idle.stderr).contains("desktop session initialization"));

    let private = Command::new(env!("CARGO_BIN_EXE_computer-use-mcp"))
        .arg("__background_worker")
        .env_remove("COMPUTER_USE_MCP_ISOLATION_MARKER")
        .output()
        .unwrap();
    assert!(!private.status.success());
    assert!(
        private.stdout.is_empty(),
        "unverified private worker must not become ready"
    );
    assert!(text(&private.stderr).contains("isolation verification failed"));
}

#[test]
fn cli_uses_routing_validation_before_starting_workers() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_computer-use-mcp"))
        .args(["call", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"[{"name":"list_desktop","arguments":{"scope":"windows","desktop":"invalid"}},{"name":"list_desktop","arguments":{"scope":"windows","desktop":"background"}}]"#).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stdout = text(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    let result: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(result["structuredContent"]["code"], "invalid_arguments");
    assert!(!text(&output.stderr).contains("desktop session initialization"));
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
