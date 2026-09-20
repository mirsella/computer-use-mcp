//! Per-process Wayland session binding and verified isolation detection.
//!
//! The server never launches compositors from Rust (see
//! `scripts/run-isolated-session.sh`).  This module resolves the display that
//! Wayland clients actually use, verifies its socket fail-closed, and reports
//! isolation only when the launcher readiness contract is still observable.
//!
//! A display name such as `wayland-virtual-1`, an environment marker, or a
//! matching diagnostic override is not evidence of isolation.  The launcher
//! writes a private-runtime marker only after it has started and checked the
//! session bus, compositor, PipeWire, portals, and AT-SPI.  Verification below
//! re-checks that marker, private sockets, process ownership evidence, and the
//! exact environment used by the clients.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

/// Explicit display override for diagnostics.  It never changes the display
/// selected by Wayland clients; a mismatch is rejected by the active-session
/// requirement instead of being logged and ignored.
pub const DISPLAY_OVERRIDE_ENV: &str = "COMPUTER_USE_MCP_DISPLAY";
/// Marker exported by the isolated-session launcher.
pub const ISOLATED_ENV: &str = "COMPUTER_USE_MCP_ISOLATED";
/// Alternate marker exported by the isolated-session launcher.
pub const VIRTUAL_ENV: &str = "COMPUTER_USE_MCP_VIRTUAL_SESSION";
/// Readiness file written only after all isolated services are verified.
pub const ISOLATION_MARKER_ENV: &str = "COMPUTER_USE_MCP_ISOLATION_MARKER";
/// Display socket name bound by the Wayland client libraries.
pub const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";
/// Directory holding the Wayland socket.
pub const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";
/// Session type; must be `wayland` for portal-backed capture/input.
pub const XDG_SESSION_TYPE_ENV: &str = "XDG_SESSION_TYPE";
/// Session-bus address used by zbus and portal clients.
pub const DBUS_SESSION_BUS_ADDRESS_ENV: &str = "DBUS_SESSION_BUS_ADDRESS";
/// Accessibility-bus address used by toolkit clients.
pub const AT_SPI_BUS_ADDRESS_ENV: &str = "AT_SPI_BUS_ADDRESS";
/// PipeWire runtime directory selected by the isolated launcher.
pub const PIPEWIRE_RUNTIME_DIR_ENV: &str = "PIPEWIRE_RUNTIME_DIR";
/// PipeWire remote selected by the isolated launcher.
pub const PIPEWIRE_REMOTE_ENV: &str = "PIPEWIRE_REMOTE";

const ISOLATION_MARKER_VERSION: &str = "1";
const VERIFIED_ISOLATION_REASON: &str = "verified private isolated session";

/// Resolved display binding for the current process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDisplay {
    /// `WAYLAND_DISPLAY` when present, otherwise the diagnostic override.
    /// The fallback is useful for reporting an incomplete environment only;
    /// active-session validation requires `wayland_display`.
    pub effective: Option<String>,
    /// Raw `WAYLAND_DISPLAY` value bound by the Wayland client libraries.
    pub wayland_display: Option<String>,
    /// Raw `COMPUTER_USE_MCP_DISPLAY` value, if set.
    pub override_display: Option<String>,
    /// True when both are set but disagree.
    pub diverged: bool,
}

/// Session description used for startup logging and `doctor` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescription {
    /// Display name actually bound by Wayland clients.
    pub display: Option<String>,
    /// Resolved socket path, when the display names one.
    pub socket: Option<PathBuf>,
    /// Whether the socket path exists on disk.
    pub socket_present: bool,
    /// True only after [`verified_isolated_session_from_env`] succeeds.
    pub isolated: bool,
    /// Why the isolation verdict was reached (stable, human-readable).
    pub isolation_reason: &'static str,
}

/// Evidence that the current process is inside the launcher's private session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIsolation {
    /// Readiness marker that supplied the session contract.
    pub marker_path: PathBuf,
    /// Private runtime directory containing the checked sockets.
    pub runtime_dir: PathBuf,
    /// Wayland display selected by clients.
    pub display: String,
    /// Private session-bus address.
    pub dbus_address: String,
    /// Private PipeWire socket.
    pub pipewire_socket: PathBuf,
}

pub(crate) fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Resolve the display binding from injected environment values.  Wayland's
/// own value wins because that is what the client libraries consume.
pub fn resolve_display(
    wayland_display: Option<String>,
    override_display: Option<String>,
) -> ResolvedDisplay {
    let wayland_display = clean(wayland_display);
    let override_display = clean(override_display);
    let diverged = match (&wayland_display, &override_display) {
        (Some(current), Some(wanted)) => current != wanted,
        _ => false,
    };
    let effective = wayland_display.clone().or(override_display.clone());
    ResolvedDisplay {
        effective,
        wayland_display,
        override_display,
        diverged,
    }
}

/// Resolve the display binding from the process environment.
pub fn resolve_display_from_env() -> ResolvedDisplay {
    resolve_display(
        std::env::var(WAYLAND_DISPLAY_ENV).ok(),
        std::env::var(DISPLAY_OVERRIDE_ENV).ok(),
    )
}

pub(crate) fn flag_is_truthy(value: Option<String>) -> bool {
    clean(value)
        .is_some_and(|flag| matches!(flag.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// A marker or a display name cannot prove that this process is isolated.
/// Call [`is_verified_isolated_session`] when the caller needs a watcher or
/// takeover decision.
pub fn is_isolated_session(
    _display: Option<&str>,
    _isolated_flag: Option<String>,
    _virtual_flag: Option<String>,
) -> bool {
    false
}

/// Isolation verdict based only on caller-supplied labels.  It intentionally
/// never returns true: filesystem, socket, and process checks require the live
/// environment and the launcher's readiness marker.
pub fn isolation_verdict_for(
    display: Option<&str>,
    isolated_flag: Option<String>,
    virtual_flag: Option<String>,
) -> (bool, &'static str) {
    if flag_is_truthy(isolated_flag) || flag_is_truthy(virtual_flag) {
        (false, "isolation marker is not proof of a private session")
    } else if display.is_some_and(display_looks_virtual) {
        (
            false,
            "virtual display name is not proof of a private session",
        )
    } else if display.is_some() {
        (false, "private-session evidence is absent")
    } else {
        (false, "no display bound")
    }
}

fn display_looks_virtual(display: &str) -> bool {
    let name = display.to_ascii_lowercase();
    name.starts_with("wayland-virtual")
        || name.starts_with("wayland-isolated")
        || name.starts_with("wayland-agent")
        || name.contains("virtual")
}

/// Verify the complete isolated-session contract from the process environment.
/// This is the only isolation predicate callers should use for takeover or
/// watcher decisions.
pub fn verified_isolated_session_from_env() -> Result<VerifiedIsolation, String> {
    verify_isolated_session(std::env::vars_os().collect())
}

/// Convenience predicate for code that only needs the safe boolean outcome.
pub fn is_verified_isolated_session() -> bool {
    verified_isolated_session_from_env().is_ok()
}

/// Isolation verdict from the process environment.
pub fn isolated_session_from_env() -> (bool, &'static str) {
    if is_verified_isolated_session() {
        (true, VERIFIED_ISOLATION_REASON)
    } else {
        let resolved = resolve_display_from_env();
        isolation_verdict_for(
            resolved.wayland_display.as_deref(),
            std::env::var(ISOLATED_ENV).ok(),
            std::env::var(VIRTUAL_ENV).ok(),
        )
    }
}

/// Resolve the socket path for a display name. Mirrors the Wayland client
/// rule: an absolute display is already a path, otherwise it names a socket
/// inside `XDG_RUNTIME_DIR` (which must be set).
pub fn wayland_socket_path(
    display: &str,
    xdg_runtime_dir: Option<&str>,
) -> Result<PathBuf, String> {
    if display.starts_with('/') {
        return Ok(PathBuf::from(display));
    }
    match clean(xdg_runtime_dir.map(str::to_owned)) {
        Some(dir) => Ok(Path::new(&dir).join(display)),
        None => Err(format!(
            "cannot resolve Wayland socket for display {display:?}: \
             {XDG_RUNTIME_DIR_ENV} is not set"
        )),
    }
}

/// Fail-closed socket check with an injectable existence probe.
pub fn check_wayland_socket(
    display: &str,
    xdg_runtime_dir: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let socket = wayland_socket_path(display, xdg_runtime_dir)?;
    if exists(&socket) {
        Ok(socket)
    } else {
        Err(format!(
            "Wayland socket for display {display:?} is missing at {}: \
             run inside the signed-in user's Linux Wayland session or start an \
             isolated virtual session with scripts/run-isolated-session.sh",
            socket.display()
        ))
    }
}

/// Fail-closed session requirement for portal-backed surfaces.  A diagnostic
/// override cannot bind clients and a mismatch is therefore an error.
pub fn require_active_session_from_env() -> Result<SessionDescription, String> {
    let mut description = require_active_session(
        std::env::var(XDG_SESSION_TYPE_ENV).ok(),
        std::env::var(WAYLAND_DISPLAY_ENV).ok(),
        std::env::var(DISPLAY_OVERRIDE_ENV).ok(),
        std::env::var(XDG_RUNTIME_DIR_ENV).ok(),
        &real_wayland_socket_exists,
    )?;

    if isolation_requested_from_env() {
        verified_isolated_session_from_env().map_err(|error| {
            format!(
                "isolated session was requested but its private-session contract is invalid: {error}"
            )
        })?;
        description.isolated = true;
        description.isolation_reason = VERIFIED_ISOLATION_REASON;
    }
    Ok(description)
}

fn require_active_session(
    session_type: Option<String>,
    wayland_display: Option<String>,
    override_display: Option<String>,
    xdg_runtime_dir: Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<SessionDescription, String> {
    let resolved = resolve_display(wayland_display, override_display);
    let Some(display) = resolved.wayland_display.clone() else {
        return Err(format!(
            "no Wayland display bound: {WAYLAND_DISPLAY_ENV} is not set; \
             {DISPLAY_OVERRIDE_ENV} is diagnostic only and cannot bind clients"
        ));
    };
    let session_type = clean(session_type);
    if session_type.as_deref() != Some("wayland") {
        return Err(format!(
            "session type is {session_type:?} (need {XDG_SESSION_TYPE_ENV}=wayland); \
             capture requires the signed-in user's Linux Wayland session"
        ));
    }
    if resolved.diverged {
        return Err(format!(
            "{DISPLAY_OVERRIDE_ENV}={:?} disagrees with {WAYLAND_DISPLAY_ENV}={:?}; \
             the override cannot change the Wayland client display",
            resolved.override_display, resolved.wayland_display
        ));
    }
    let socket = check_wayland_socket(&display, xdg_runtime_dir.as_deref(), exists)?;
    Ok(SessionDescription {
        display: Some(display),
        socket: Some(socket),
        socket_present: true,
        isolated: false,
        isolation_reason: "private-session evidence is absent",
    })
}

fn isolation_requested_from_env() -> bool {
    flag_is_truthy(std::env::var(ISOLATED_ENV).ok())
        || flag_is_truthy(std::env::var(VIRTUAL_ENV).ok())
        || std::env::var_os(ISOLATION_MARKER_ENV).is_some()
}

fn real_wayland_socket_exists(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

/// Best-effort session description for logging and `doctor`; never fails.
pub fn describe_session_from_env() -> SessionDescription {
    let resolved = resolve_display_from_env();
    let (isolated, reason) = isolated_session_from_env();
    let xdg_runtime_dir = std::env::var(XDG_RUNTIME_DIR_ENV).ok();
    let display = resolved.wayland_display;
    let (socket, socket_present) = display
        .as_deref()
        .and_then(|display| {
            wayland_socket_path(display, xdg_runtime_dir.as_deref())
                .ok()
                .map(|path| {
                    let present = real_wayland_socket_exists(&path);
                    (path, present)
                })
        })
        .map(|(path, present)| (Some(path), present))
        .unwrap_or((None, false));
    SessionDescription {
        display,
        socket,
        socket_present,
        isolated,
        isolation_reason: reason,
    }
}

/// Log the bound display and verified isolation state; never fails.
pub fn log_session_description() {
    let description = describe_session_from_env();
    match description.display.as_deref() {
        Some(display) => eprintln!(
            "computer-use-mcp: Wayland display={display} isolated={} ({})",
            description.isolated, description.isolation_reason
        ),
        None => eprintln!(
            "computer-use-mcp: no Wayland display bound (AT-SPI-only calls still work; \
             portal capture/input will refuse until a session is available)"
        ),
    }
}

fn verify_isolated_session(
    environment: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> Result<VerifiedIsolation, String> {
    #[cfg(not(unix))]
    {
        let _ = environment;
        return Err("isolated-session verification requires Unix socket metadata".into());
    }

    #[cfg(unix)]
    {
        if !flag_is_truthy(environment_value(&environment, ISOLATED_ENV)) {
            return Err(format!("{ISOLATED_ENV} is not enabled"));
        }

        let marker_path = required_environment_path(&environment, ISOLATION_MARKER_ENV)?;
        let marker_metadata = std::fs::symlink_metadata(&marker_path).map_err(|error| {
            format!(
                "cannot read isolation marker {}: {error}",
                marker_path.display()
            )
        })?;
        require_private_file(&marker_metadata, "isolation marker")?;
        let fields = parse_marker(&marker_path)?;
        if fields.get("version").map(String::as_str) != Some(ISOLATION_MARKER_VERSION) {
            return Err("unsupported isolation marker version".into());
        }

        let runtime_dir = required_environment_path(&environment, XDG_RUNTIME_DIR_ENV)?;
        let marker_runtime = required_marker_path(&fields, "runtime_dir")?;
        if marker_runtime != runtime_dir {
            return Err(format!(
                "marker runtime {} does not match {XDG_RUNTIME_DIR_ENV} {}",
                marker_runtime.display(),
                runtime_dir.display()
            ));
        }
        let runtime_metadata = std::fs::symlink_metadata(&runtime_dir).map_err(|error| {
            format!(
                "cannot read private runtime {}: {error}",
                runtime_dir.display()
            )
        })?;
        require_private_directory(&runtime_metadata, "private runtime")?;
        if !marker_path.starts_with(&runtime_dir) {
            return Err("isolation marker is outside the private runtime".into());
        }
        if marker_path != runtime_dir.join("isolation.ready") {
            return Err("isolation marker is not the launcher's readiness marker".into());
        }

        let display = required_marker_value(&fields, "wayland_display")?;
        let resolved = resolve_display(
            environment_value(&environment, WAYLAND_DISPLAY_ENV),
            environment_value(&environment, DISPLAY_OVERRIDE_ENV),
        );
        if resolved.diverged || resolved.wayland_display.as_deref() != Some(display.as_str()) {
            return Err("marker display does not match the Wayland client display".into());
        }
        if display.contains('/') {
            return Err("isolated Wayland display must be a private runtime socket name".into());
        }
        if environment.contains_key(std::ffi::OsStr::new("DISPLAY"))
            || environment.contains_key(std::ffi::OsStr::new("XAUTHORITY"))
        {
            return Err("physical X11 display variables are still present".into());
        }
        if clean(environment_value(&environment, XDG_SESSION_TYPE_ENV)).as_deref()
            != Some("wayland")
        {
            return Err(format!("{XDG_SESSION_TYPE_ENV} is not wayland"));
        }
        if !desktop_is_kde(environment_value(&environment, "XDG_CURRENT_DESKTOP")) {
            return Err("isolated session is not identified as KDE".into());
        }

        let dbus_address = required_marker_value(&fields, "dbus_address")?;
        let expected_dbus = format!("unix:path={}", runtime_dir.join("bus").display());
        if dbus_address != expected_dbus
            || environment_value(&environment, DBUS_SESSION_BUS_ADDRESS_ENV).as_deref()
                != Some(dbus_address.as_str())
        {
            return Err("session bus is not the private runtime bus".into());
        }

        let atspi_address = required_marker_value(&fields, "at_spi_bus_address")?;
        let expected_atspi = format!("unix:path={}", runtime_dir.join("at-spi/bus").display());
        if atspi_address != expected_atspi
            || environment_value(&environment, AT_SPI_BUS_ADDRESS_ENV).as_deref()
                != Some(atspi_address.as_str())
        {
            return Err("AT-SPI is not using the private accessibility bus".into());
        }

        if required_marker_value(&fields, "pipewire_remote")? != "pipewire-0"
            || environment_value(&environment, PIPEWIRE_REMOTE_ENV).as_deref() != Some("pipewire-0")
            || environment_value(&environment, PIPEWIRE_RUNTIME_DIR_ENV).as_deref()
                != Some(runtime_dir.to_string_lossy().as_ref())
        {
            return Err("PipeWire is not using the private runtime".into());
        }

        let wayland_socket = wayland_socket_path(&display, Some(&runtime_dir.to_string_lossy()))?;
        require_socket(&wayland_socket, "Wayland")?;
        let pipewire_socket = required_marker_path(&fields, "pipewire_socket")?;
        if pipewire_socket != runtime_dir.join("pipewire-0") {
            return Err("marker PipeWire socket is not in the private runtime".into());
        }
        require_socket(&pipewire_socket, "PipeWire")?;
        require_socket(&runtime_dir.join("bus"), "D-Bus")?;
        require_socket(&runtime_dir.join("at-spi/bus"), "AT-SPI")?;

        let pid_fields = [
            "launcher_pid",
            "dbus_pid",
            "compositor_pid",
            "pipewire_pid",
            "wireplumber_pid",
            "atspi_bus_pid",
            "atspi_registry_pid",
            "portal_backend_pid",
            "portal_pid",
        ];
        let mut pids = BTreeMap::new();
        for field in pid_fields {
            let pid = required_pid(&fields, field)?;
            if pids.insert(pid, field).is_some() {
                return Err("isolation marker reuses a process id".into());
            }
            // The launcher shell exports the private environment after its own
            // exec, so /proc reports its original ambient environment.  Its
            // ownership/liveness is enough; every service supervisor is
            // exec'd with and checked against the private environment.
            if field == "launcher_pid" {
                require_owned_process(pid, field)?;
            } else {
                require_process_environment(pid, &environment, field)?;
            }
        }

        Ok(VerifiedIsolation {
            marker_path,
            runtime_dir,
            display,
            dbus_address,
            pipewire_socket,
        })
    }
}

fn environment_value(
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
    key: &str,
) -> Option<String> {
    environment
        .get(std::ffi::OsStr::new(key))
        .map(|value| value.to_string_lossy().into_owned())
}

fn required_environment_path(
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
    key: &str,
) -> Result<PathBuf, String> {
    let value = environment_value(environment, key).ok_or_else(|| format!("{key} is not set"))?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!("{key} must be an absolute path"));
    }
    Ok(path)
}

fn required_marker_value(fields: &BTreeMap<String, String>, key: &str) -> Result<String, String> {
    fields
        .get(key)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("isolation marker is missing {key}"))
}

fn required_marker_path(fields: &BTreeMap<String, String>, key: &str) -> Result<PathBuf, String> {
    let value = required_marker_value(fields, key)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!(
            "isolation marker field {key} must be an absolute path"
        ));
    }
    Ok(path)
}

fn required_pid(fields: &BTreeMap<String, String>, key: &str) -> Result<u32, String> {
    let value = required_marker_value(fields, key)?;
    let pid = value
        .parse::<u32>()
        .map_err(|_| format!("isolation marker field {key} is not a pid"))?;
    if pid == 0 {
        return Err(format!("isolation marker field {key} is zero"));
    }
    Ok(pid)
}

fn parse_marker(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read isolation marker {}: {error}", path.display()))?;
    if contents.len() > 16 * 1024 {
        return Err("isolation marker is unexpectedly large".into());
    }
    let mut fields = BTreeMap::new();
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Err("isolation marker contains a malformed line".into());
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err("isolation marker contains an invalid field name".into());
        }
        if fields.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("isolation marker repeats field {key}"));
        }
    }
    Ok(fields)
}

#[cfg(unix)]
fn require_private_directory(metadata: &std::fs::Metadata, label: &str) -> Result<(), String> {
    if !metadata.is_dir() {
        return Err(format!("{label} is not a directory"));
    }
    if metadata.uid() != current_process_uid()? {
        return Err(format!("{label} is not owned by the current user"));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!("{label} is accessible by group or other users"));
    }
    Ok(())
}

#[cfg(unix)]
fn require_private_file(metadata: &std::fs::Metadata, label: &str) -> Result<(), String> {
    if !metadata.is_file() {
        return Err(format!("{label} is not a regular file"));
    }
    if metadata.uid() != current_process_uid()? {
        return Err(format!("{label} is not owned by the current user"));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!("{label} is accessible by group or other users"));
    }
    Ok(())
}

#[cfg(unix)]
fn require_socket(path: &Path, label: &str) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{label} socket {} is unavailable: {error}", path.display()))?;
    if !metadata.file_type().is_socket() {
        return Err(format!(
            "{label} path {} is not a Unix socket",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn require_owned_process(pid: u32, label: &str) -> Result<(), String> {
    let process_metadata = std::fs::symlink_metadata(format!("/proc/{pid}"))
        .map_err(|error| format!("{label} process {pid} is unavailable: {error}"))?;
    if process_metadata.uid() != current_process_uid()? {
        return Err(format!(
            "{label} process {pid} is not owned by the current user"
        ));
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("{label} process {pid} is not readable: {error}"))?;
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, fields)| fields.chars().next())
        .ok_or_else(|| format!("{label} process {pid} has malformed status"))?;
    if state == 'Z' {
        return Err(format!("{label} process {pid} is no longer alive"));
    }
    Ok(())
}

#[cfg(unix)]
fn require_process_environment(
    pid: u32,
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
    label: &str,
) -> Result<(), String> {
    require_owned_process(pid, label)?;
    let process_environment = std::fs::read(format!("/proc/{pid}/environ"))
        .map_err(|error| format!("{label} process {pid} is not readable: {error}"))?;
    let expected = [
        (ISOLATED_ENV, environment_value(environment, ISOLATED_ENV)),
        (
            ISOLATION_MARKER_ENV,
            environment_value(environment, ISOLATION_MARKER_ENV),
        ),
        (
            XDG_SESSION_TYPE_ENV,
            environment_value(environment, XDG_SESSION_TYPE_ENV),
        ),
        (
            "XDG_CURRENT_DESKTOP",
            environment_value(environment, "XDG_CURRENT_DESKTOP"),
        ),
        (
            XDG_RUNTIME_DIR_ENV,
            environment_value(environment, XDG_RUNTIME_DIR_ENV),
        ),
        (
            WAYLAND_DISPLAY_ENV,
            environment_value(environment, WAYLAND_DISPLAY_ENV),
        ),
        (
            DBUS_SESSION_BUS_ADDRESS_ENV,
            environment_value(environment, DBUS_SESSION_BUS_ADDRESS_ENV),
        ),
        (
            AT_SPI_BUS_ADDRESS_ENV,
            environment_value(environment, AT_SPI_BUS_ADDRESS_ENV),
        ),
        (
            PIPEWIRE_RUNTIME_DIR_ENV,
            environment_value(environment, PIPEWIRE_RUNTIME_DIR_ENV),
        ),
        (
            PIPEWIRE_REMOTE_ENV,
            environment_value(environment, PIPEWIRE_REMOTE_ENV),
        ),
    ];
    for (key, value) in expected {
        let Some(value) = value else {
            return Err(format!("{key} is missing from the launcher environment"));
        };
        let needle = format!("{key}={value}");
        if !process_environment
            .split(|byte| *byte == 0)
            .any(|entry| entry == needle.as_bytes())
        {
            return Err(format!(
                "{label} process {pid} does not belong to the private session"
            ));
        }
    }
    for key in [
        "DISPLAY",
        "XAUTHORITY",
        "DBUS_STARTER_ADDRESS",
        "DBUS_STARTER_BUS_TYPE",
        "DBUS_SESSION_BUS_PID",
        "DBUS_SESSION_BUS_WINDOWID",
        "DBUS_SYSTEM_BUS_ADDRESS",
    ] {
        let prefix = format!("{key}=");
        if process_environment
            .split(|byte| *byte == 0)
            .any(|entry| entry.starts_with(prefix.as_bytes()))
        {
            return Err(format!(
                "{label} process {pid} still exposes physical session variable {key}"
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn current_process_uid() -> Result<u32, String> {
    std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|error| format!("cannot determine current process owner: {error}"))
}

fn desktop_is_kde(value: Option<String>) -> bool {
    value.is_some_and(|value| {
        value
            .split([':', ';'])
            .any(|part| matches!(part.trim().to_ascii_lowercase().as_str(), "kde" | "plasma"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_display_wins_and_divergence_is_rejected_later() {
        let resolved = resolve_display(Some("wayland-0".into()), Some("wayland-virtual-1".into()));
        assert_eq!(resolved.effective.as_deref(), Some("wayland-0"));
        assert!(resolved.diverged);

        let aligned = resolve_display(Some("wayland-0".into()), Some("wayland-0".into()));
        assert!(!aligned.diverged);

        let plain = resolve_display(Some("wayland-0".into()), None);
        assert_eq!(plain.effective.as_deref(), Some("wayland-0"));
        assert!(!plain.diverged);

        let blank = resolve_display(Some("  ".into()), Some(String::new()));
        assert_eq!(blank.effective, None);
    }

    #[test]
    fn labels_and_names_never_prove_isolation() {
        assert!(!is_isolated_session(
            Some("wayland-0"),
            Some("1".into()),
            None
        ));
        assert!(!is_isolated_session(
            Some("wayland-virtual-123"),
            None,
            None
        ));
        assert!(!is_isolated_session(Some("wayland-isolated-0"), None, None));
        assert!(!is_isolated_session(
            Some("wayland-0"),
            None,
            Some("true".into())
        ));
        assert!(!is_isolated_session(None, None, None));

        let verdict = isolation_verdict_for(Some("wayland-virtual-1"), None, None);
        assert!(!verdict.0);
        assert!(verdict.1.contains("not proof"));
    }

    #[test]
    fn socket_path_mirrors_wayland_client_lookup() {
        assert_eq!(
            wayland_socket_path("/tmp/custom-wayland", None).as_deref(),
            Ok(Path::new("/tmp/custom-wayland"))
        );
        assert_eq!(
            wayland_socket_path("wayland-virtual-1", Some("/run/user/1000")),
            Ok(PathBuf::from("/run/user/1000/wayland-virtual-1"))
        );
        assert!(wayland_socket_path("wayland-0", None).is_err());
        assert!(wayland_socket_path("wayland-0", Some("  ")).is_err());
    }

    #[test]
    fn missing_socket_fails_closed() {
        let error = check_wayland_socket("wayland-0", Some("/run/user/1000"), &|_| false)
            .expect_err("missing socket must fail closed");
        assert!(error.contains("wayland-0"));
        assert!(error.contains("run-isolated-session.sh"));
        assert!(check_wayland_socket("wayland-0", Some("/run/user/1000"), &|_| true).is_ok());
    }

    #[test]
    fn session_requirement_rejects_ambiguous_display_binding() {
        let missing_display = require_active_session(
            Some("wayland".into()),
            None,
            Some("wayland-virtual-1".into()),
            Some("/run/user/1000".into()),
            &|_| true,
        );
        assert!(
            missing_display
                .expect_err("diagnostic override cannot bind clients")
                .contains("WAYLAND_DISPLAY")
        );

        let mismatch = require_active_session(
            Some("wayland".into()),
            Some("wayland-0".into()),
            Some("wayland-virtual-1".into()),
            Some("/run/user/1000".into()),
            &|_| true,
        );
        assert!(
            mismatch
                .expect_err("mismatched display must fail closed")
                .contains("disagrees")
        );

        let wrong_type = require_active_session(
            Some("x11".into()),
            Some("wayland-0".into()),
            None,
            Some("/run/user/1000".into()),
            &|_| true,
        );
        assert!(
            wrong_type
                .expect_err("must fail closed")
                .contains("XDG_SESSION_TYPE")
        );

        let missing_socket = require_active_session(
            Some("wayland".into()),
            Some("wayland-0".into()),
            None,
            Some("/run/user/1000".into()),
            &|_| false,
        );
        assert!(
            missing_socket
                .expect_err("must fail closed")
                .contains("missing")
        );
    }

    #[test]
    fn kde_detection_is_explicit() {
        assert!(desktop_is_kde(Some("KDE".into())));
        assert!(desktop_is_kde(Some("plasma:KDE".into())));
        assert!(!desktop_is_kde(Some("GNOME".into())));
        assert!(!desktop_is_kde(None));
    }
}
