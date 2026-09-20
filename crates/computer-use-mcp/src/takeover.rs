//! Human-takeover detection and session handoff.
//!
//! If a human grabs physical input during an agent session, the agent must
//! stop safely instead of fighting the user for the pointer and keyboard.
//!
//! Best-effort and fail-open for this feature only: when no takeover signal
//! is observable the feature is disabled and normal operation continues. A
//! missing or denied signal mechanism never blocks `act` or `wait_for`.
//!
//! Mechanisms, in order:
//! 1. Physical input watcher (supported): an operation-owned thread polls readable
//!    `/dev/input/event*` character devices for key/relative/absolute
//!    activity. Agent input flows through the compositor's virtual EIS
//!    channel and never appears in `/dev/input`, so any such event is human
//!    (or another physical seat actor) and latches the takeover signal.
//!    Device permissions must allow reads. Missing devices and poll failures
//!    are retried while the operation is active. Idle sessions and isolated
//!    displays never watch physical devices. Dropping the operation joins
//!    its watcher thread.
//! 2. Cooperative handoff signal (supported): `COMPUTER_USE_MCP_TAKEOVER=1`
//!    or a handoff file (`COMPUTER_USE_MCP_TAKEOVER_FILE`, defaulting to
//!    `$XDG_RUNTIME_DIR/computer-use-mcp-takeover`). An operator creates the
//!    signal to request handoff; in-flight execution observes it and aborts
//!    with `UserTakeoverInterrupted`, attempting held-input cleanup and
//!    restoration of any unfinished desktop hunt.
//! 3. EIS physical-modifier refusal mapping: the EIS backend already refuses
//!    generated input while physical Ctrl/Alt/Super or latched modifiers are
//!    held; those refusals are surfaced as `UserTakeoverInterrupted`.
//!
//! Takeover stays latched for the lifetime of this MCP. To resume, the user
//! must authorize a restart, clear any cooperative signal, restart the MCP,
//! and obtain a fresh observation. Removing the signal alone never resumes
//! an interrupted agent. Errors report dispatch and cleanup separately;
//! interruption cannot undo input already delivered.
//!
//! The org.freedesktop.portal.InputCapture interface is deliberately NOT
//! used: it is a pointer-barrier capture API for synergy-style apps that
//! needs capture zones plus a user-consent flow, which is the wrong tool for
//! passive takeover detection.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::session::{clean, flag_is_truthy};

/// Forces takeover mode when truthy (`1`/`true`/`yes`).
pub const TAKEOVER_ENV: &str = "COMPUTER_USE_MCP_TAKEOVER";
/// Overrides the handoff file path.
pub const TAKEOVER_FILE_ENV: &str = "COMPUTER_USE_MCP_TAKEOVER_FILE";
/// Default handoff file name inside `XDG_RUNTIME_DIR`.
pub const TAKEOVER_FILE_NAME: &str = "computer-use-mcp-takeover";
/// Overrides physical-device discovery with colon-separated paths (for
/// tests). When set, these paths are watched as-is; otherwise readable
/// `/dev/input/event*` character devices are enumerated.
pub const INPUT_DEVICES_ENV: &str = "COMPUTER_USE_MCP_INPUT_DEVICES";
/// Default directory scanned for physical input devices.
pub const INPUT_DEVICES_DIR: &str = "/dev/input";
/// Byte size of one Linux `input_event` struct: 16 bytes of timeval plus
/// `u16` type, `u16` code, and `u32` value.
pub const INPUT_EVENT_SIZE: usize = 24;
/// Upper bound on simultaneously watched devices; bounds fd usage on hosts
/// with many virtual inputs.
const MAX_WATCHED_DEVICES: usize = 64;
/// How long `poll` sleeps between device-set rescans while devices are open
/// (also the hotplug granularity), and how long the loop sleeps when nothing
/// is open yet.
const POLL_RESCAN_INTERVAL: Duration = Duration::from_millis(50);
const EMPTY_RESCAN_INTERVAL: Duration = Duration::from_millis(50);

/// Linux `input_event` types relevant to takeover detection.
pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;

/// Resolve the handoff file path from injected environment values. Pure and
/// deterministic so tests never touch the process environment.
pub fn takeover_file_path(
    custom_path: Option<String>,
    xdg_runtime_dir: Option<String>,
) -> Option<PathBuf> {
    if let Some(custom) = clean(custom_path) {
        return Some(PathBuf::from(custom));
    }
    clean(xdg_runtime_dir).map(|dir| Path::new(&dir).join(TAKEOVER_FILE_NAME))
}

/// Handoff file path from the process environment.
pub fn takeover_file_path_from_env() -> Option<PathBuf> {
    takeover_file_path(
        std::env::var(TAKEOVER_FILE_ENV).ok(),
        std::env::var(crate::session::XDG_RUNTIME_DIR_ENV).ok(),
    )
}

/// True when a cooperative handoff signal is observable. Pure and
/// deterministic; `exists` probes the filesystem.
pub fn takeover_signal_active(
    takeover_flag: Option<String>,
    custom_path: Option<String>,
    xdg_runtime_dir: Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> bool {
    if flag_is_truthy(takeover_flag) {
        return true;
    }
    takeover_file_path(custom_path, xdg_runtime_dir).is_some_and(|path| exists(&path))
}

/// Cooperative handoff signal from the process environment and filesystem.
pub fn takeover_requested() -> bool {
    takeover_signal_active(
        std::env::var(TAKEOVER_ENV).ok(),
        std::env::var(TAKEOVER_FILE_ENV).ok(),
        std::env::var(crate::session::XDG_RUNTIME_DIR_ENV).ok(),
        &|path| path.exists(),
    )
}

/// Matches the EIS backend's physical-input refusals (see
/// `input::eis::validate_physical_modifiers`). A human holding real modifiers
/// while the agent types is treated as a takeover, not a backend glitch.
pub fn is_physical_input_refusal(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("physical latched modifier")
        || normalized.contains("physical ctrl, alt, or super")
}

/// One parsed Linux `input_event`. The timestamp is diagnostic only; the
/// filter decision uses [`is_takeover_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub time_sec: i64,
    pub time_usec: i64,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// Parse one 24-byte native-endian `input_event` (16-byte timeval, `u16`
/// type, `u16` code, `u32` value). Pure; no I/O, no unsafe.
pub fn parse_input_event(bytes: &[u8; INPUT_EVENT_SIZE]) -> InputEvent {
    let time_sec = i64::from_ne_bytes(bytes[0..8].try_into().expect("time_sec slice is 8 bytes"));
    let time_usec =
        i64::from_ne_bytes(bytes[8..16].try_into().expect("time_usec slice is 8 bytes"));
    InputEvent {
        time_sec,
        time_usec,
        kind: u16::from_ne_bytes(bytes[16..18].try_into().expect("kind slice is 2 bytes")),
        code: u16::from_ne_bytes(bytes[18..20].try_into().expect("code slice is 2 bytes")),
        value: i32::from_ne_bytes(bytes[20..24].try_into().expect("value slice is 4 bytes")),
    }
}

/// Presses/repeats and nonzero relative movement indicate manipulation.
/// Releases and zero relative movement do not. Absolute zero is a valid
/// coordinate, so absolute axes are compared against their previous value
/// by the device drain rather than treating zero as noise.
pub fn is_takeover_event(event: &InputEvent) -> bool {
    match event.kind {
        EV_KEY => event.value > 0,
        EV_REL => event.value != 0,
        EV_ABS => true,
        _ => false,
    }
}

/// Split an injected `COMPUTER_USE_MCP_INPUT_DEVICES` value into paths.
/// Returns `None` when unset or blank (fall back to the default `/dev/input`
/// scan); `Some` (possibly empty after filtering blanks) when explicitly set.
pub fn device_paths_from_override(value: Option<String>) -> Option<Vec<PathBuf>> {
    let value = clean(value)?;
    Some(
        value
            .split(':')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(PathBuf::from)
            .collect(),
    )
}

/// Override device list from the process environment.
pub fn device_paths_from_env() -> Option<Vec<PathBuf>> {
    device_paths_from_override(std::env::var(INPUT_DEVICES_ENV).ok())
}

/// Enumerate default physical input devices: `/dev/input/event*` character
/// devices, sorted for determinism. Non-character entries are skipped so
/// mice/js aggregates and symlinks never join the watch set. Fail-open: any
/// scan error yields an empty list.
fn default_device_paths() -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(INPUT_DEVICES_DIR) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("event") {
            continue;
        }
        let is_char = entry
            .file_type()
            .is_ok_and(|kind| std::os::unix::fs::FileTypeExt::is_char_device(&kind));
        if !is_char {
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();
    paths
}

/// Resolve the device set to watch: explicit override when set, otherwise
/// the default `/dev/input` scan.
pub fn resolve_device_paths() -> Vec<PathBuf> {
    device_paths_from_env().unwrap_or_else(default_device_paths)
}

/// Count how many of `paths` can be opened for nonblocking read. Opens are
/// immediately closed; this is a side-effect-free probe shared by arming and
/// `doctor`.
pub fn probe_device_paths(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|path| open_input_device(path).is_ok())
        .count()
}

/// Open one input device `O_RDONLY|O_NONBLOCK|O_CLOEXEC` via rustix.
fn open_input_device(path: &Path) -> Result<std::fs::File, rustix::io::Errno> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(std::fs::File::from(fd))
}

/// Best-effort hardware watcher state for `doctor` and arming diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareWatcherStatus {
    Watching { devices: usize },
    Disabled { reason: &'static str },
}

/// Probe the current hardware watcher state without spawning anything:
/// `Watching` when at least one device opens, otherwise fail-open
/// `Disabled` with a static reason.
pub fn hardware_watcher_status() -> HardwareWatcherStatus {
    hardware_watcher_status_for_paths(&resolve_device_paths())
}

/// [`hardware_watcher_status`] over an explicit path list (test seam; the
/// production path always goes through [`resolve_device_paths`]).
pub fn hardware_watcher_status_for_paths(paths: &[PathBuf]) -> HardwareWatcherStatus {
    let openable = probe_device_paths(paths);
    if openable > 0 {
        HardwareWatcherStatus::Watching { devices: openable }
    } else if device_paths_from_env().is_some() {
        HardwareWatcherStatus::Disabled {
            reason: "no COMPUTER_USE_MCP_INPUT_DEVICES path is readable",
        }
    } else {
        HardwareWatcherStatus::Disabled {
            reason: "no readable /dev/input/event* nodes (need input group membership?)",
        }
    }
}

/// Process-local takeover latch. Observing the cooperative signal latches
/// it; removing the file does not silently resume an interrupted agent.
#[derive(Debug, Default)]
pub struct TakeoverMonitor {
    manual: AtomicBool,
    hardware: Arc<AtomicBool>,
    watcher_armed: AtomicBool,
}

impl TakeoverMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        if takeover_requested() {
            self.manual.store(true, Ordering::Release);
        }
        self.manual.load(Ordering::Acquire) || self.hardware.load(Ordering::Acquire)
    }

    /// Enable operation-scoped hardware monitoring in shared sessions only.
    /// Called by the production constructor; no devices are opened here.
    pub fn arm_hardware_watcher(&self) {
        self.watcher_armed.store(
            !crate::session::is_verified_isolated_session(),
            Ordering::Release,
        );
    }

    /// Monitor only a currently owned mutation or wait. Idle desktop use must
    /// not disable the next call. A detected takeover stays latched until the
    /// operator restarts the MCP after authorizing resume.
    pub(crate) fn begin_operation(&self) -> Option<HardwareWatch> {
        if !self.watcher_armed.load(Ordering::Acquire) || self.is_active() {
            return None;
        }
        let paths = resolve_device_paths();
        // Only the default scan is rescanned for hotplug; an explicit
        // override set is stable by definition.
        let rescan = device_paths_from_env().is_none();
        match hardware_watcher_status_for_paths(&paths) {
            HardwareWatcherStatus::Watching { devices } => {
                eprintln!(
                    "computer-use-mcp: hardware takeover watcher watching {devices} input device(s)"
                );
            }
            HardwareWatcherStatus::Disabled { reason } => {
                eprintln!(
                    "computer-use-mcp: hardware takeover watcher unavailable ({reason}); retrying while operation is active"
                );
            }
        }
        spawn_hardware_watcher(Arc::clone(&self.hardware), paths, rescan)
    }

    pub(crate) async fn interrupted(&self) {
        loop {
            if self.is_active() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(crate) fn latch(&self) {
        self.manual.store(true, Ordering::Release);
    }

    /// Latch a handoff request in-process. Test-only: production detection
    /// reads the live cooperative signal; nothing in the server latches.
    #[cfg(test)]
    pub(crate) fn trip(&self) {
        self.manual.store(true, Ordering::Release);
    }

    /// Clear a latched handoff request. Test-only; see [`Self::trip`].
    #[cfg(test)]
    pub(crate) fn reset(&self) {
        self.manual.store(false, Ordering::Release);
    }
}

struct WatchedDevice {
    path: PathBuf,
    file: std::fs::File,
    carry: Vec<u8>,
    absolute: std::collections::HashMap<u16, i32>,
}

impl WatchedDevice {
    fn open(path: &Path) -> Option<Self> {
        open_input_device(path).ok().map(|file| Self {
            path: path.to_owned(),
            file,
            carry: Vec::new(),
            absolute: Default::default(),
        })
    }
}

/// Dropping an operation stops and joins its watcher, including empty-device
/// retry loops. No thread or device descriptor survives the operation.
pub(crate) struct HardwareWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HardwareWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("computer-use-mcp: takeover watcher panicked");
        }
    }
}

enum DrainOutcome {
    Quiet,
    Takeover(InputEvent),
    Dead,
}

/// Spawn an operation-owned watcher thread over `paths`. When `rescan`
/// is set the default `/dev/input` scan is recomputed every pass so
/// hotplugged devices join within one interval; explicit path sets are
/// stable and only reopened when their fds die. Any error (including
/// thread-spawn failure) disables the watcher with one stderr line and
/// leaves `latch` untouched: fail-open.
pub(crate) fn spawn_hardware_watcher(
    latch: Arc<AtomicBool>,
    paths: Vec<PathBuf>,
    rescan: bool,
) -> Option<HardwareWatch> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    // Open synchronously, before execution can dispatch input, so events
    // arriving before the worker is scheduled are queued on these fds.
    let devices = paths
        .iter()
        .take(MAX_WATCHED_DEVICES)
        .filter_map(|path| WatchedDevice::open(path))
        .collect();
    let result = std::thread::Builder::new()
        .name("takeover-input-watch".to_owned())
        .spawn(move || watch_loop(&latch, &worker_stop, paths, devices, rescan));
    match result {
        Ok(thread) => Some(HardwareWatch {
            stop,
            thread: Some(thread),
        }),
        Err(error) => {
            eprintln!(
                "computer-use-mcp: hardware takeover watcher disabled (thread spawn failed: {error})"
            );
            None
        }
    }
}

/// Watch loop: open missing devices, `poll` for readability, drain complete
/// 24-byte events, and latch on the first key/relative/absolute event. The
/// loop exits once the latch is set; dead fds are dropped silently and, when
/// `rescan` is set, the device set is recomputed about every
/// [`POLL_RESCAN_INTERVAL`] for hotplug.
fn watch_loop(
    latch: &AtomicBool,
    stop: &AtomicBool,
    initial: Vec<PathBuf>,
    mut devices: Vec<WatchedDevice>,
    rescan: bool,
) {
    let mut wanted = initial;
    let mut logged_poll_error = false;
    loop {
        if latch.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
            return;
        }
        // (Re)open anything wanted that is not already open, up to the cap.
        if rescan {
            wanted = default_device_paths();
        }
        for path in &wanted {
            if devices.len() >= MAX_WATCHED_DEVICES {
                break;
            }
            if devices.iter().any(|device| device.path == *path) {
                continue;
            }
            if let Some(device) = WatchedDevice::open(path) {
                devices.push(device);
            }
        }
        if devices.is_empty() {
            std::thread::sleep(EMPTY_RESCAN_INTERVAL);
            continue;
        }
        {
            use rustix::event::{PollFd, PollFlags};
            // Collect readiness first so the immutable poll borrow ends
            // before devices are drained mutably below.
            let ready: Vec<PollFlags> = {
                let mut polls: Vec<PollFd<'_>> = devices
                    .iter()
                    .map(|device| {
                        PollFd::new(
                            &device.file,
                            PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
                        )
                    })
                    .collect();
                match rustix::event::poll(&mut polls, Some(&poll_timeout(POLL_RESCAN_INTERVAL))) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => {
                        if !logged_poll_error {
                            eprintln!(
                                "computer-use-mcp: hardware takeover watcher poll failed ({error}); retrying"
                            );
                            logged_poll_error = true;
                        }
                        std::thread::sleep(EMPTY_RESCAN_INTERVAL);
                        continue;
                    }
                    Ok(_) => {}
                }
                polls.iter().map(|poll| poll.revents()).collect()
            };
            let mut dead = Vec::new();
            let mut tripped = false;
            for (index, ready) in ready.iter().enumerate() {
                if !ready.contains(PollFlags::IN)
                    && !ready.contains(PollFlags::ERR)
                    && !ready.contains(PollFlags::HUP)
                {
                    continue;
                }
                match drain_device(&mut devices[index], stop) {
                    DrainOutcome::Takeover(event) => {
                        eprintln!(
                            "computer-use-mcp: human takeover from {} (type={} code={} value={})",
                            devices[index].path.display(),
                            event.kind,
                            event.code,
                            event.value
                        );
                        tripped = true;
                        break;
                    }
                    DrainOutcome::Dead => dead.push(index),
                    DrainOutcome::Quiet => {}
                }
            }
            if tripped {
                latch.store(true, Ordering::Release);
                return;
            }
            for index in dead.into_iter().rev() {
                devices.swap_remove(index);
            }
        }
    }
}

/// Drain all currently available bytes from one device, parsing complete
/// 24-byte events. `Takeover` on the first key/rel/abs event, `Dead` when
/// the fd errors or hits EOF (evdev nodes never EOF while present; dropping
/// also keeps regular-file probes from busy-spinning on constant readiness).
fn drain_device(device: &mut WatchedDevice, stop: &AtomicBool) -> DrainOutcome {
    let mut buffer = [0_u8; INPUT_EVENT_SIZE * 32];
    loop {
        if stop.load(Ordering::Acquire) {
            return DrainOutcome::Quiet;
        }
        match device.file.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return DrainOutcome::Quiet;
            }
            Err(_) | Ok(0) => return DrainOutcome::Dead,
            Ok(consumed) => {
                device.carry.extend_from_slice(&buffer[..consumed]);
                while device.carry.len() >= INPUT_EVENT_SIZE {
                    let chunk: [u8; INPUT_EVENT_SIZE] = device.carry[..INPUT_EVENT_SIZE]
                        .try_into()
                        .expect("carry holds a full event");
                    device.carry.drain(..INPUT_EVENT_SIZE);
                    let event = parse_input_event(&chunk);
                    if event.kind == EV_ABS
                        && device.absolute.insert(event.code, event.value) == Some(event.value)
                    {
                        continue;
                    }
                    if is_takeover_event(&event) {
                        return DrainOutcome::Takeover(event);
                    }
                }
            }
        }
    }
}

fn poll_timeout(duration: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: duration.subsec_nanos() as _,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn absent(_: &Path) -> bool {
        false
    }

    fn present(_: &Path) -> bool {
        true
    }

    #[test]
    fn env_flag_forces_takeover_without_filesystem() {
        assert!(takeover_signal_active(
            Some("1".into()),
            None,
            None,
            &absent
        ));
        assert!(takeover_signal_active(
            Some("yes".into()),
            None,
            None,
            &absent
        ));
        assert!(!takeover_signal_active(
            Some("0".into()),
            None,
            None,
            &present
        ));
        assert!(!takeover_signal_active(None, None, None, &present));
    }

    #[test]
    fn handoff_file_path_prefers_explicit_override() {
        assert_eq!(
            takeover_file_path(Some("/tmp/handoff".into()), Some("/run/user/1".into())),
            Some(PathBuf::from("/tmp/handoff"))
        );
        assert_eq!(
            takeover_file_path(None, Some("/run/user/1".into())),
            Some(PathBuf::from("/run/user/1/computer-use-mcp-takeover"))
        );
        assert_eq!(takeover_file_path(None, None), None);
    }

    #[test]
    fn handoff_file_presence_requests_takeover() {
        assert!(takeover_signal_active(
            None,
            None,
            Some("/run/user/1".into()),
            &present
        ));
        assert!(!takeover_signal_active(
            None,
            None,
            Some("/run/user/1".into()),
            &absent
        ));
    }

    #[test]
    fn physical_modifier_refusals_map_to_takeover() {
        assert!(is_physical_input_refusal(
            "a physical latched modifier is active; refusing generated keyboard input"
        ));
        assert!(is_physical_input_refusal(
            "physical Ctrl, Alt, or Super is active; refusing generated keyboard input"
        ));
        assert!(!is_physical_input_refusal(
            "generated input requires a live screenshot provider"
        ));
        assert!(!is_physical_input_refusal(""));
    }

    #[test]
    fn manual_latch_trips_and_resets() {
        let monitor = TakeoverMonitor::new();
        // The live environment signal must stay quiet for a deterministic
        // latch test; a set takeover flag/file would fail this test loudly
        // rather than flake.
        assert!(
            !takeover_requested(),
            "test environment must not assert a takeover signal"
        );
        assert!(!monitor.is_active());
        monitor.trip();
        assert!(monitor.is_active());
        monitor.reset();
        assert!(!monitor.is_active());
    }

    /// Build one synthetic native-endian 24-byte `input_event`.
    fn encode_event(kind: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_SIZE] {
        let mut bytes = [0_u8; INPUT_EVENT_SIZE];
        bytes[0..8].copy_from_slice(&0_i64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&0_i64.to_ne_bytes());
        bytes[16..18].copy_from_slice(&kind.to_ne_bytes());
        bytes[18..20].copy_from_slice(&code.to_ne_bytes());
        bytes[20..24].copy_from_slice(&value.to_ne_bytes());
        bytes
    }

    #[test]
    fn input_event_parses_native_endian_fields() {
        let event = parse_input_event(&encode_event(EV_KEY, 30, 1));
        assert_eq!(event.kind, EV_KEY);
        assert_eq!(event.code, 30);
        assert_eq!(event.value, 1);
        let mut bytes = encode_event(EV_REL, 0, -7);
        bytes[0..8].copy_from_slice(&1_700_000_000_i64.to_ne_bytes());
        let moved = parse_input_event(&bytes);
        assert_eq!(moved.time_sec, 1_700_000_000);
        assert_eq!(moved.kind, EV_REL);
        assert_eq!(moved.value, -7);
    }

    #[test]
    fn takeover_filter_triggers_only_on_key_rel_abs() {
        // Presses, releases, motion, and absolute positioning all latch:
        // agent EIS input never reaches /dev/input, so any of these is human.
        for (kind, code, value) in [
            (EV_KEY, 30, 1),
            (EV_KEY, 30, 2),
            (EV_REL, 0, 5),
            (EV_REL, 1, -3),
            (EV_ABS, 0, 1024),
            (EV_ABS, 1, 0),
        ] {
            let event = parse_input_event(&encode_event(kind, code, value));
            assert!(
                is_takeover_event(&event),
                "kind={kind} code={code} value={value} must latch"
            );
        }
        // Framing and status traffic never latches.
        for (kind, code, value) in [
            (EV_SYN, 0, 0),
            (EV_KEY, 30, 0),
            (EV_REL, 0, 0),
            (EV_SYN, 1, 0),
            (4, 4, 1),
            (17, 0, 1),
            (20, 0, 1),
            (5, 0, 0),
        ] {
            let event = parse_input_event(&encode_event(kind, code, value));
            assert!(
                !is_takeover_event(&event),
                "kind={kind} code={code} value={value} must not latch"
            );
        }
    }

    #[test]
    fn override_paths_split_colons_and_ignore_blanks() {
        assert_eq!(device_paths_from_override(None), None);
        assert_eq!(device_paths_from_override(Some("   ".into())), None);
        assert_eq!(
            device_paths_from_override(Some("/dev/input/event0:/tmp/a:: /tmp/b ".into())),
            Some(vec![
                PathBuf::from("/dev/input/event0"),
                PathBuf::from("/tmp/a"),
                PathBuf::from("/tmp/b"),
            ])
        );
    }

    #[test]
    fn status_reports_disabled_without_openable_devices() {
        // Assert on the variant only: the exact disabled reason depends on
        // whether the override env var is set, which a sibling test mutates.
        for paths in [
            Vec::new(),
            vec![PathBuf::from(
                "/nonexistent/computer-use-mcp-takeover-test-node",
            )],
        ] {
            match hardware_watcher_status_for_paths(&paths) {
                HardwareWatcherStatus::Disabled { .. } => {}
                HardwareWatcherStatus::Watching { .. } => {
                    panic!("unopenable device paths must not report watching")
                }
            }
        }
    }

    #[test]
    fn default_scan_never_panics_and_stays_sorted() {
        let paths = default_device_paths();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    fn unique_scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "computer-use-mcp-takeover-test-{}-{}.bin",
            std::process::id(),
            name
        ))
    }

    fn wait_for_active(monitor: &TakeoverMonitor, bound: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < bound {
            if monitor.is_active() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        monitor.is_active()
    }

    #[test]
    fn watcher_trips_latch_from_prefilled_file() {
        let path = unique_scratch("press");
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_event(EV_SYN, 0, 0));
        payload.extend_from_slice(&encode_event(EV_KEY, 30, 1));
        std::fs::write(&path, &payload).expect("write synthetic events");
        let monitor = TakeoverMonitor::new();
        assert!(!monitor.is_active());
        let _watch =
            spawn_hardware_watcher(Arc::clone(&monitor.hardware), vec![path.clone()], false);
        assert!(
            wait_for_active(&monitor, Duration::from_secs(5)),
            "prefilled EV_KEY bytes must trip the hardware latch"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn watcher_ignores_syn_only_prefill_and_empty_set() {
        // SYN-only traffic must never latch.
        let path = unique_scratch("syn");
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_event(EV_SYN, 0, 0));
        payload.extend_from_slice(&encode_event(EV_SYN, 1, 0));
        std::fs::write(&path, &payload).expect("write synthetic syn events");
        let monitor = TakeoverMonitor::new();
        let _watch =
            spawn_hardware_watcher(Arc::clone(&monitor.hardware), vec![path.clone()], false);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !monitor.is_active(),
            "EV_SYN traffic must not trip the hardware latch"
        );
        let _ = std::fs::remove_file(&path);

        // An empty device set is fail-open disabled, never latched.
        let idle = TakeoverMonitor::new();
        let _idle_watch = spawn_hardware_watcher(Arc::clone(&idle.hardware), Vec::new(), false);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!idle.is_active());
    }

    #[test]
    fn status_reports_watching_for_readable_path() {
        // `hardware_watcher_status` itself reads the process environment, so
        // the hermetic seam is `hardware_watcher_status_for_paths` with an
        // explicit path. Any readable file opens O_RDONLY (device-ness is
        // only filtered during the default /dev/input scan, not for
        // explicit paths), which also covers the override-env branch.
        let path = unique_scratch("watching");
        std::fs::write(&path, encode_event(EV_REL, 0, 2)).expect("write synthetic events");
        let status = hardware_watcher_status_for_paths(std::slice::from_ref(&path));
        let _ = std::fs::remove_file(&path);
        assert_eq!(status, HardwareWatcherStatus::Watching { devices: 1 });
    }

    #[test]
    fn watcher_retries_an_initially_missing_device_and_stops_on_drop() {
        let path = unique_scratch("hotplug-event");
        assert!(!path.exists());
        let monitor = TakeoverMonitor::new();
        let watch =
            spawn_hardware_watcher(Arc::clone(&monitor.hardware), vec![path.clone()], false)
                .unwrap();
        std::thread::sleep(Duration::from_millis(70));
        std::fs::write(&path, encode_event(EV_KEY, 30, 1)).unwrap();
        assert!(wait_for_active(&monitor, Duration::from_secs(1)));
        drop(watch);
        std::fs::remove_file(path).unwrap();

        let watch =
            spawn_hardware_watcher(Arc::new(AtomicBool::new(false)), Vec::new(), false).unwrap();
        let stopped = Arc::clone(&watch.stop);
        let before = Instant::now();
        drop(watch);
        assert!(stopped.load(Ordering::Acquire));
        assert!(before.elapsed() < Duration::from_secs(1));
        assert_eq!(Arc::strong_count(&stopped), 1, "worker thread has exited");
    }
}
