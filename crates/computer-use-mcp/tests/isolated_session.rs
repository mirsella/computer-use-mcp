//! Isolated-session launcher tests.
//!
//! The service fixture binds real Unix sockets and records its environment,
//! but it does not connect to or mutate the user's physical desktop.  The
//! launcher is therefore exercised through the same startup and teardown paths
//! used in production without requiring a live KWin session in CI.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/run-isolated-session.sh")
}

fn fixture_source() -> &'static str {
    include_str!("fixtures/isolated-session-stub.sh")
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "computer-use-mcp-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&path).expect("test temporary directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos()
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("fixture executable bit");
}

fn create_fixture(root: &Path) -> PathBuf {
    let fixture = root.join("fixture.sh");
    fs::write(&fixture, fixture_source()).expect("write service fixture");
    make_executable(&fixture);

    for role in [
        "dbus-daemon",
        "dbus-send",
        "kwin_wayland",
        "pipewire",
        "wireplumber",
        "xdg-desktop-portal",
        "xdg-desktop-portal-kde",
        "at-spi-bus-launcher",
        "at-spi2-registryd",
        "server",
    ] {
        let link = root.join(role);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&fixture, &link).expect("fixture role link");
    }
    fixture
}

fn harness(fail_role: Option<&str>) -> (TempDir, Command) {
    let temp = TempDir::new("isolated-session");
    let env_dir = temp.path.join("env");
    fs::create_dir(&env_dir).expect("fixture environment directory");
    let pid_log = temp.path.join("pids");
    let fixture = create_fixture(&temp.path);

    let mut command = Command::new("bash");
    command
        .arg(script_path())
        .arg("--width")
        .arg("640")
        .arg("--height")
        .arg("480")
        .arg("--scale")
        .arg("1")
        .arg("--")
        .arg("this-command-must-not-run")
        .arg("--fixture-argument")
        .env("COMPUTER_USE_MCP_BIN", fixture.with_file_name("server"))
        .env("COMPUTER_USE_MCP_STARTUP_TIMEOUT_SECS", "3")
        .env("STUB_ENV_DIR", &env_dir)
        .env("STUB_PID_LOG", &pid_log)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/physical/session-bus",
        )
        .env("AT_SPI_BUS_ADDRESS", "unix:path=/physical/at-spi-bus")
        .env("DISPLAY", ":99")
        .env("XAUTHORITY", "/physical/xauthority")
        .env_remove("KWIN_WAYLAND_NO_PERMISSION_CHECKS");

    for (variable, role) in [
        ("COMPUTER_USE_MCP_DBUS_DAEMON_BIN", "dbus-daemon"),
        ("COMPUTER_USE_MCP_DBUS_SEND_BIN", "dbus-send"),
        ("COMPUTER_USE_MCP_COMPOSITOR_BIN", "kwin_wayland"),
        ("COMPUTER_USE_MCP_PIPEWIRE_BIN", "pipewire"),
        ("COMPUTER_USE_MCP_WIREPLUMBER_BIN", "wireplumber"),
        ("COMPUTER_USE_MCP_PORTAL_BIN", "xdg-desktop-portal"),
        (
            "COMPUTER_USE_MCP_PORTAL_BACKEND_BIN",
            "xdg-desktop-portal-kde",
        ),
        ("COMPUTER_USE_MCP_ATSPI_BUS_BIN", "at-spi-bus-launcher"),
        ("COMPUTER_USE_MCP_ATSPI_REGISTRY_BIN", "at-spi2-registryd"),
    ] {
        command.env(variable, temp.path.join(role));
    }
    if let Some(role) = fail_role {
        command.env("STUB_FAIL_ROLE", role);
    }
    (temp, command)
}

fn run(mut command: Command) -> Output {
    command.output().expect("isolated-session launcher process")
}

fn read_file(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join("env").join(name)).expect("fixture environment log")
}

fn runtime_from_environment(log: &str) -> PathBuf {
    log.lines()
        .find_map(|line| line.strip_prefix("XDG_RUNTIME_DIR="))
        .map(PathBuf::from)
        .expect("private runtime in server environment")
}

fn recorded_pids(root: &Path) -> Vec<u32> {
    fs::read_to_string(root.join("pids"))
        .expect("fixture pid log")
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|pid| pid.parse().expect("numeric fixture pid"))
        .collect()
}

fn recorded_arguments(root: &Path, role: &str) -> Vec<String> {
    fs::read_to_string(root.join("env").join(format!("{role}.argv")))
        .expect("fixture argument log")
        .lines()
        .filter_map(|line| line.strip_prefix("argv="))
        .map(str::to_owned)
        .collect()
}

fn assert_processes_gone(pids: &[u32]) {
    for _ in 0..50 {
        if pids
            .iter()
            .all(|pid| !Path::new("/proc").join(pid.to_string()).exists())
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for pid in pids {
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "launcher-owned fixture process {pid} survived cleanup"
        );
    }
}

#[test]
fn isolated_session_script_is_present_and_executable() {
    let script = script_path();
    assert!(
        script.is_file(),
        "missing isolated-session launcher: {}",
        script.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script)
            .expect("launcher metadata")
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "launcher must stay executable: {}",
            script.display()
        );
    }
}

#[test]
fn isolated_session_script_passes_syntax_check() {
    let output = Command::new("bash")
        .arg("-n")
        .arg(script_path())
        .output()
        .expect("bash must be available for the launcher syntax check");
    assert!(
        output.status.success(),
        "bash -n failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn isolated_session_runs_override_in_a_private_environment_and_cleans_up() {
    let (temp, command) = harness(None);
    let unrelated = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("unrelated process");
    let mut unrelated = Some(unrelated);
    let output = run(command);
    let server_environment = read_file(&temp.path, "server");
    let runtime = runtime_from_environment(&server_environment);

    assert!(
        output.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(server_environment.contains("server_arg=--fixture-argument"));
    assert!(server_environment.contains("COMPUTER_USE_MCP_ISOLATED=1"));
    assert!(server_environment.contains("XDG_SESSION_TYPE=wayland"));
    assert!(server_environment.contains("XDG_CURRENT_DESKTOP=KDE"));
    assert!(server_environment.contains("WAYLAND_DISPLAY=wayland-virtual-"));
    assert!(server_environment.contains(&format!(
        "COMPUTER_USE_MCP_ISOLATION_MARKER={}/isolation.ready",
        runtime.display()
    )));
    assert!(server_environment.contains(&format!(
        "DBUS_SESSION_BUS_ADDRESS=unix:path={}/bus",
        runtime.display()
    )));
    assert!(!server_environment.contains("/physical/session-bus"));
    assert!(server_environment.contains(&format!(
        "AT_SPI_BUS_ADDRESS=unix:path={}/at-spi/bus",
        runtime.display()
    )));
    assert!(!server_environment.contains("/physical/at-spi-bus"));
    assert!(server_environment.contains("PIPEWIRE_REMOTE=pipewire-0"));
    assert!(server_environment.contains(&format!("PIPEWIRE_RUNTIME_DIR={}", runtime.display())));
    assert!(server_environment.contains("DISPLAY=<unset>"));
    assert!(server_environment.contains("XAUTHORITY=<unset>"));
    for variable in [
        "DBUS_STARTER_ADDRESS",
        "DBUS_STARTER_BUS_TYPE",
        "DBUS_SESSION_BUS_PID",
        "DBUS_SESSION_BUS_WINDOWID",
        "DBUS_SYSTEM_BUS_ADDRESS",
    ] {
        assert!(
            server_environment.contains(&format!("{variable}=<unset>")),
            "server inherited {variable}"
        );
    }
    for service in [
        "dbus-daemon",
        "kwin_wayland",
        "pipewire",
        "wireplumber",
        "at-spi-bus-launcher",
        "at-spi2-registryd",
        "xdg-desktop-portal-kde",
        "xdg-desktop-portal",
    ] {
        let service_environment = read_file(&temp.path, service);
        assert!(
            service_environment.contains(&format!(
                "DBUS_SESSION_BUS_ADDRESS=unix:path={}/bus",
                runtime.display()
            )),
            "{service} did not inherit the private session bus"
        );
        assert!(
            service_environment.contains(&format!(
                "AT_SPI_BUS_ADDRESS=unix:path={}/at-spi/bus",
                runtime.display()
            )),
            "{service} did not inherit the private AT-SPI bus"
        );
        assert!(
            service_environment.contains(&format!("XDG_RUNTIME_DIR={}", runtime.display())),
            "{service} did not inherit the private runtime"
        );
        assert!(
            service_environment.contains(&format!(
                "COMPUTER_USE_MCP_ISOLATION_MARKER={}/isolation.ready",
                runtime.display()
            )),
            "{service} did not inherit the readiness marker path"
        );
        assert!(
            service_environment.contains("DISPLAY=<unset>")
                && service_environment.contains("XAUTHORITY=<unset>")
                && service_environment.contains("DBUS_STARTER_ADDRESS=<unset>")
                && service_environment.contains("DBUS_STARTER_BUS_TYPE=<unset>")
                && service_environment.contains("DBUS_SESSION_BUS_PID=<unset>")
                && service_environment.contains("DBUS_SESSION_BUS_WINDOWID=<unset>")
                && service_environment.contains("DBUS_SYSTEM_BUS_ADDRESS=<unset>"),
            "{service} inherited physical X11 variables"
        );
    }
    assert!(!runtime.exists(), "private runtime survived normal cleanup");
    assert_processes_gone(&recorded_pids(&temp.path));

    let unrelated = unrelated.as_mut().expect("unrelated process handle");
    assert!(
        unrelated
            .try_wait()
            .expect("unrelated process status")
            .is_none()
    );
    unrelated.kill().expect("stop unrelated process");
    unrelated.wait().expect("reap unrelated process");
}

#[test]
fn isolated_session_failure_cleans_only_owned_processes() {
    let (temp, command) = harness(Some("xdg-desktop-portal-kde"));
    let unrelated = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("unrelated process");
    let mut unrelated = Some(unrelated);
    let output = run(command);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("portal_backend"),
        "unexpected failure: {stderr}"
    );
    assert_processes_gone(&recorded_pids(&temp.path));

    let unrelated = unrelated.as_mut().expect("unrelated process handle");
    assert!(
        unrelated
            .try_wait()
            .expect("unrelated process status")
            .is_none()
    );
    unrelated.kill().expect("stop unrelated process");
    unrelated.wait().expect("reap unrelated process");
}

#[test]
fn isolated_session_rejects_missing_required_dependency_before_startup() {
    let (temp, mut command) = harness(None);
    command.env(
        "COMPUTER_USE_MCP_PORTAL_BACKEND_BIN",
        temp.path.join("does-not-exist"),
    );
    let output = run(command);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("xdg-desktop-portal-kde"),
        "unexpected error: {stderr}"
    );
    assert!(
        !temp.path.join("pids").exists(),
        "services started before dependency validation"
    );
}

#[test]
fn isolated_session_starts_atspi_services_with_supported_arguments() {
    let (temp, command) = harness(None);
    let output = run(command);
    assert!(
        output.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The private accessibility bus must be started with arguments the
    // installed launcher accepts.  An unsupported flag aborts the launcher
    // before it binds its socket, so the private session never becomes ready.
    let launcher_arguments = recorded_arguments(&temp.path, "at-spi-bus-launcher");
    assert_eq!(
        launcher_arguments,
        vec!["--launch-immediately".to_owned()],
        "at-spi-bus-launcher must be started with only the supported flag"
    );
    let registry_arguments = recorded_arguments(&temp.path, "at-spi2-registryd");
    assert_eq!(
        registry_arguments,
        vec![
            "--dbus-name".to_owned(),
            "org.a11y.atspi.Registry".to_owned()
        ],
        "at-spi2-registryd must own the registry name on the private bus"
    );
}

#[test]
fn isolated_session_enables_and_verifies_private_accessibility_status() {
    let (temp, command) = harness(None);
    let output = run(command);
    assert!(output.status.success());
    let calls = read_file(&temp.path, "dbus-send.calls");
    assert!(calls.contains("org.freedesktop.DBus.Properties.Set string:org.a11y.Status string:IsEnabled variant:boolean:true"));
    assert!(
        calls.contains(
            "org.freedesktop.DBus.Properties.Get string:org.a11y.Status string:IsEnabled"
        )
    );
}

#[test]
fn isolated_session_rejects_disabled_accessibility_before_server_start() {
    let (temp, mut command) = harness(None);
    command.env("STUB_ATSPI_ENABLED", "false");
    let output = run(command);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("private AT-SPI status remained disabled")
    );
    assert!(!temp.path.join("env/server").exists());
    assert_processes_gone(&recorded_pids(&temp.path));
}

#[test]
fn isolated_session_enables_private_kde_rich_window_management_by_default() {
    let (temp, command) = harness(None);
    let output = run(command);
    assert!(
        output.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let server_environment = read_file(&temp.path, "server");
    assert!(
        server_environment.contains("COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT=1"),
        "the private runner must enable KDE rich window management by default"
    );
    assert!(read_file(&temp.path, "kwin_wayland").contains("KWIN_WAYLAND_NO_PERMISSION_CHECKS=1"));
    assert!(server_environment.contains("KWIN_WAYLAND_NO_PERMISSION_CHECKS=<unset>"));
}

#[test]
fn isolated_session_preserves_explicit_kde_rich_window_management_value() {
    for value in ["0", "false", ""] {
        let (temp, mut command) = harness(None);
        command.env("COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT", value);
        let output = run(command);
        assert!(
            output.status.success(),
            "launcher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            read_file(&temp.path, "server")
                .contains(&format!("COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT={value}\n"))
        );
        assert!(
            read_file(&temp.path, "kwin_wayland").contains("KWIN_WAYLAND_NO_PERMISSION_CHECKS=0")
        );
    }
}

#[test]
fn isolated_session_script_has_no_unsupported_or_heuristic_fallbacks() {
    let content = fs::read_to_string(script_path()).expect("launcher must remain readable");
    for marker in [
        "set -euo pipefail",
        "COMPUTER_USE_MCP_ISOLATED=1",
        "COMPUTER_USE_MCP_ISOLATION_MARKER",
        "DBUS_SESSION_BUS_ADDRESS",
        "AT_SPI_BUS_ADDRESS",
        "PIPEWIRE_REMOTE=pipewire-0",
        "--virtual",
    ] {
        assert!(
            content.contains(marker),
            "launcher lost required marker {marker:?}"
        );
    }
    assert!(!content.contains("cage"));
    assert!(!content.contains("gamescope"));
    assert!(!content.contains("systemctl --user"));
    assert!(!content.contains("--systemd-activation"));
    for forbidden in ["PASSWORD=", "API_KEY", "BEGIN PRIVATE", "read -s"] {
        assert!(
            !content.contains(forbidden),
            "launcher must not handle secrets ({forbidden:?})"
        );
    }
}
