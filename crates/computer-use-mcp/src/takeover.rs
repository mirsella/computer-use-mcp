//! Human-takeover detection and session handoff.
//!
//! If a human grabs physical input during an agent session, the agent must
//! stop safely instead of fighting the user for the pointer and keyboard.
//!
//! Best-effort and fail-open for this feature only: when no takeover signal
//! is observable the feature is disabled and normal operation continues. A
//! missing or denied signal mechanism never blocks an action. An idle wait
//! reports unavailable monitoring rather than establishing physical idle.
//!
//! Mechanisms, in order:
//! 1. Physical input watcher (supported): a runtime-owned thread polls readable
//!    `/dev/input/event*` character devices for key/relative/absolute
//!    activity. Agent input flows through the compositor's virtual EIS
//!    channel and never appears in `/dev/input`, so any such event is human
//!    (or another physical seat actor) and starts a 60-second quiet period.
//!    Device permissions must allow reads. Missing devices and poll failures
//!    are retried for the lifetime of the foreground runtime, including between
//!    calls. Verified isolated displays never watch physical devices. Runtime
//!    shutdown stops and joins the watcher.
//! 2. Cooperative handoff signal (supported): `COMPUTER_USE_MCP_TAKEOVER=1`
//!    or a handoff file (`COMPUTER_USE_MCP_TAKEOVER_FILE`, defaulting to
//!    `$XDG_RUNTIME_DIR/computer-use-mcp-takeover`). An operator creates the
//!    signal to request handoff; in-flight execution observes it and aborts
//!    with `HumanInputBusy`, attempting held-input cleanup and
//!    restoration of any unfinished desktop hunt.
//! 3. EIS physical-modifier refusal mapping: the EIS backend already refuses
//!    generated input while physical Ctrl/Alt/Super or latched modifiers are
//!    held; those refusals are surfaced as `HumanInputBusy`.
//!
//! Foreground mutations fail immediately while busy. `wait_for human_idle`
//! waits explicitly for the quiet period, without replaying interrupted input.
//! Cooperative signals block while asserted and start a quiet period when
//! cleared. Resumption needs a fresh observation, not an MCP restart.
//! Errors report dispatch and cleanup separately;
//! interruption cannot undo input already delivered.
//!
//! The org.freedesktop.portal.InputCapture interface is deliberately NOT
//! used: it is a pointer-barrier capture API for synergy-style apps that
//! needs capture zones plus a user-consent flow, which is the wrong tool for
//! passive takeover detection.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Read as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::time::Instant;

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
/// Foreground input may resume only after this much quiet following activity.
pub const HUMAN_IDLE_INTERVAL: Duration = Duration::from_secs(60);

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

#[derive(Debug)]
enum CooperativeSignal {
    Always,
    File(PathBuf),
    Disabled,
}

impl CooperativeSignal {
    fn resolve(
        flag: Option<String>,
        custom_path: Option<String>,
        runtime_dir: Option<String>,
    ) -> Self {
        if flag_is_truthy(flag) {
            Self::Always
        } else {
            takeover_file_path(custom_path, runtime_dir).map_or(Self::Disabled, Self::File)
        }
    }

    fn from_env() -> Self {
        Self::resolve(
            std::env::var(TAKEOVER_ENV).ok(),
            std::env::var(TAKEOVER_FILE_ENV).ok(),
            std::env::var(crate::session::XDG_RUNTIME_DIR_ENV).ok(),
        )
    }

    fn requested(&self, exists: impl Fn(&Path) -> bool) -> bool {
        match self {
            Self::Always => true,
            Self::File(path) => exists(path),
            Self::Disabled => false,
        }
    }
}

/// Cooperative handoff signal from the process environment and filesystem.
pub fn takeover_requested() -> bool {
    CooperativeSignal::from_env().requested(Path::exists)
}

/// Matches the EIS backend's physical-input refusals (see
/// `input::eis::validate_physical_modifiers`). A human holding real modifiers
/// while the agent types is treated as a takeover, not a backend glitch.
pub fn is_physical_input_refusal(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("physical latched modifier")
        || normalized.contains("physical ctrl, alt, or super")
}

/// The relevant fields of a Linux `input_event`; kernel timestamps are unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// Parse one 24-byte native-endian `input_event` (16-byte timeval, `u16`
/// type, `u16` code, `u32` value). Pure; no I/O, no unsafe.
pub fn parse_input_event(bytes: &[u8; INPUT_EVENT_SIZE]) -> InputEvent {
    InputEvent {
        kind: u16::from_ne_bytes(bytes[16..18].try_into().expect("kind slice is 2 bytes")),
        code: u16::from_ne_bytes(bytes[18..20].try_into().expect("code slice is 2 bytes")),
        value: i32::from_ne_bytes(bytes[20..24].try_into().expect("value slice is 4 bytes")),
    }
}

/// Key transitions/repeats and nonzero relative movement indicate manipulation.
/// Absolute zero is a valid
/// coordinate, so absolute axes are compared against their previous value
/// by the device drain rather than treating zero as noise.
pub fn is_takeover_event(event: &InputEvent) -> bool {
    match event.kind {
        EV_KEY => event.value >= 0,
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
    let mut seen = HashSet::new();
    Some(
        value
            .split(':')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter(|part| seen.insert(*part))
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

/// Count paths that support the same read and held-key queries as the watcher.
/// Doctor closes these probes immediately; arming uses the actual open devices.
pub fn probe_device_paths(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|path| WatchedDevice::open(path).is_ok())
        .take(MAX_WATCHED_DEVICES)
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

#[derive(Debug, Default)]
struct PhysicalActivity {
    last_activity: Option<Instant>,
    devices: HashMap<PathBuf, HashSet<u16>>,
    cooperative: bool,
    generation: u64,
}

impl PhysicalActivity {
    fn record(&mut self, now: Instant) {
        self.last_activity = Some(now);
        self.generation = self
            .generation
            .checked_add(1)
            .expect("human input generation exhausted");
    }

    fn event(&mut self, path: &Path, event: InputEvent, now: Instant) {
        if event.kind == EV_KEY {
            let held = self
                .devices
                .get_mut(path)
                .expect("event requires a watched device");
            if event.value == 0 {
                held.remove(&event.code);
            } else {
                held.insert(event.code);
            }
        }
        self.record(now);
    }

    fn disconnect(&mut self, path: &Path) {
        let held = self
            .devices
            .remove(path)
            .expect("disconnect requires a watched device");
        if !held.is_empty() {
            self.record(Instant::now());
        }
    }

    fn connect(&mut self, path: &Path, held: HashSet<u16>) {
        let busy = !held.is_empty();
        assert!(
            self.devices.insert(path.to_owned(), held).is_none(),
            "device already watched"
        );
        if busy {
            self.record(Instant::now());
        }
    }

    fn clear_devices(&mut self) {
        if self.devices.values().any(|held| !held.is_empty()) {
            self.record(Instant::now());
        }
        self.devices.clear();
    }

    fn status(&mut self, cooperative: bool, now: Instant) -> HumanInputStatus {
        if cooperative != self.cooperative {
            self.cooperative = cooperative;
            self.record(now);
        }
        let remaining = self.last_activity.map_or(Duration::ZERO, |last| {
            HUMAN_IDLE_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        });
        let state = if cooperative {
            HumanInputState::HandoffRequested
        } else if self.devices.values().any(|held| !held.is_empty()) {
            HumanInputState::PhysicalInputHeld
        } else if !remaining.is_zero() {
            HumanInputState::QuietPeriod
        } else {
            HumanInputState::Idle
        };
        HumanInputStatus {
            state,
            remaining,
            available: !self.devices.is_empty(),
            // The phase makes observations taken during a pause stale on idle,
            // without a separate transition latch or synthetic activity event.
            generation: (self.generation, state),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HumanInputState {
    #[default]
    Idle,
    HandoffRequested,
    PhysicalInputHeld,
    QuietPeriod,
}

impl HumanInputState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::HandoffRequested => "handoff_requested",
            Self::PhysicalInputHeld => "physical_input_held",
            Self::QuietPeriod => "quiet_period",
        }
    }
}

pub(crate) type HumanInputGeneration = (u64, HumanInputState);

#[derive(Debug, Clone, Copy)]
pub(crate) struct HumanInputStatus {
    pub state: HumanInputState,
    pub remaining: Duration,
    pub available: bool,
    pub generation: HumanInputGeneration,
}

impl HumanInputStatus {
    pub(crate) fn busy(self) -> bool {
        self.state != HumanInputState::Idle
    }
}

/// Recent physical activity and a live cooperative handoff request. Each
/// interrupted operation remains aborted; the monitor itself can become idle.
#[derive(Debug)]
pub struct TakeoverMonitor {
    activity: Arc<Mutex<PhysicalActivity>>,
    watcher: Mutex<Option<HardwareWatch>>,
    signal: CooperativeSignal,
    #[cfg(test)]
    manual: AtomicBool,
}

impl Default for TakeoverMonitor {
    fn default() -> Self {
        Self {
            activity: Arc::default(),
            watcher: Mutex::default(),
            // Process configuration is fixed; only file presence is polled.
            signal: CooperativeSignal::from_env(),
            #[cfg(test)]
            manual: AtomicBool::default(),
        }
    }
}

impl TakeoverMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.status().busy()
    }

    pub(crate) fn status(&self) -> HumanInputStatus {
        let cooperative = self.signal.requested(Path::exists);
        #[cfg(test)]
        let cooperative = cooperative || self.manual.load(Ordering::Acquire);
        self.activity
            .lock()
            .expect("human input state poisoned")
            .status(cooperative, Instant::now())
    }

    /// Start persistent hardware monitoring in shared sessions only.
    pub fn arm_hardware_watcher(&self) {
        if crate::session::is_verified_isolated_session() {
            return;
        }
        let mut watcher = self.watcher.lock().expect("human input watcher poisoned");
        if watcher.is_some() {
            return;
        }
        let paths = device_paths_from_env().map_or(DevicePaths::System, DevicePaths::Fixed);
        *watcher = spawn_hardware_watcher(Arc::clone(&self.activity), paths);
    }

    pub(crate) async fn interrupted(&self) {
        loop {
            if self.is_active() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(crate) async fn wait_for_idle(&self, deadline: Instant) -> HumanInputStatus {
        loop {
            let status = self.status();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !status.busy() || remaining.is_zero() {
                return status;
            }
            tokio::time::sleep(POLL_RESCAN_INTERVAL.min(remaining)).await;
        }
    }

    pub(crate) fn record_activity(&self) {
        self.activity
            .lock()
            .expect("human input state poisoned")
            .record(Instant::now());
    }

    pub(crate) fn stop(&self) {
        // Joining must not hold the activity mutex used by the worker.
        self.watcher
            .lock()
            .expect("human input watcher poisoned")
            .take();
    }

    /// Inject physical activity without touching process environment or devices.
    #[cfg(test)]
    pub(crate) fn trip(&self) {
        self.record_activity();
    }

    /// Inject a readable watcher without opening physical devices.
    #[cfg(test)]
    pub(crate) fn make_available(&self) {
        self.activity
            .lock()
            .expect("human input state poisoned")
            .devices
            .insert(PathBuf::from("synthetic-watcher"), HashSet::new());
    }
}

struct WatchedDevice {
    path: PathBuf,
    file: std::fs::File,
    carry: Vec<u8>,
    absolute: std::collections::HashMap<u16, i32>,
}

impl WatchedDevice {
    fn open(path: &Path) -> std::io::Result<(Self, HashSet<u16>)> {
        let file = open_input_device(path)?;
        // Query keys already held before the watcher opens. evdev supplies the
        // safe ioctl API; this crate keeps unsafe_code forbidden. Non-character
        // override paths are the synthetic file/FIFO seam used by tests.
        let held = if std::os::unix::fs::FileTypeExt::is_char_device(&file.metadata()?.file_type())
        {
            let device = evdev::raw_stream::RawDevice::try_from(file.try_clone()?)?;
            if device.supported_keys().is_some() {
                device
                    .get_key_state()?
                    .iter()
                    .map(|key| key.code())
                    .collect()
            } else {
                HashSet::new()
            }
        } else {
            HashSet::new()
        };
        Ok((
            Self {
                path: path.to_owned(),
                file,
                carry: Vec::new(),
                absolute: Default::default(),
            },
            held,
        ))
    }
}

/// Runtime shutdown or drop joins the watcher, including empty-device retries.
#[derive(Debug)]
pub(crate) struct HardwareWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    activity: Arc<Mutex<PhysicalActivity>>,
}

impl Drop for HardwareWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("computer-use-mcp: takeover watcher panicked");
        }
        self.activity
            .lock()
            .expect("human input state poisoned")
            .clear_devices();
    }
}

enum DrainOutcome {
    Quiet,
    Dead,
}

enum DevicePaths {
    System,
    Fixed(Vec<PathBuf>),
}

impl DevicePaths {
    fn current(&self) -> Cow<'_, [PathBuf]> {
        match self {
            Self::System => Cow::Owned(default_device_paths()),
            Self::Fixed(paths) => Cow::Borrowed(paths),
        }
    }
}

/// Spawn a runtime-owned watcher thread. The system device set is rescanned so
/// hotplugged devices join within one interval; explicit path sets are
/// stable and only reopened when their fds die. Thread-spawn failure disables
/// monitoring and clears held state that can no longer be tracked. Detected
/// recent activity still expires normally.
fn spawn_hardware_watcher(
    activity: Arc<Mutex<PhysicalActivity>>,
    paths: DevicePaths,
) -> Option<HardwareWatch> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    // Open synchronously, before execution can dispatch input, so events
    // arriving before the worker is scheduled are queued on these fds.
    let mut devices = Vec::new();
    open_missing_devices(&paths.current(), &mut devices, &activity);
    if devices.is_empty() {
        eprintln!(
            "computer-use-mcp: hardware takeover watcher has no readable input devices; retrying while foreground runtime is alive"
        );
    } else {
        eprintln!(
            "computer-use-mcp: hardware takeover watcher watching {} input device(s)",
            devices.len()
        );
    }
    let worker_activity = Arc::clone(&activity);
    let result = std::thread::Builder::new()
        .name("takeover-input-watch".to_owned())
        .spawn(move || watch_loop(&worker_activity, &worker_stop, paths, devices));
    match result {
        Ok(thread) => Some(HardwareWatch {
            stop,
            thread: Some(thread),
            activity,
        }),
        Err(error) => {
            let mut state = activity.lock().expect("human input state poisoned");
            state.clear_devices();
            eprintln!(
                "computer-use-mcp: hardware takeover watcher disabled (thread spawn failed: {error})"
            );
            None
        }
    }
}

fn open_missing_devices(
    paths: &[PathBuf],
    devices: &mut Vec<WatchedDevice>,
    activity: &Mutex<PhysicalActivity>,
) {
    for path in paths {
        if devices.len() >= MAX_WATCHED_DEVICES {
            break;
        }
        if devices.iter().any(|device| device.path == *path) {
            continue;
        }
        if let Ok((device, held)) = WatchedDevice::open(path) {
            activity
                .lock()
                .expect("human input state poisoned")
                .connect(path, held);
            devices.push(device);
        }
    }
}

/// Watch loop: open missing devices, `poll` for readability, drain complete
/// 24-byte events, and keep tracking activity throughout busy and idle periods.
/// Dead fds are dropped; system device discovery retries each poll interval.
fn watch_loop(
    activity: &Mutex<PhysicalActivity>,
    stop: &AtomicBool,
    paths: DevicePaths,
    mut devices: Vec<WatchedDevice>,
) {
    let mut logged_poll_error = false;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // (Re)open anything wanted that is not already open, up to the cap.
        let wanted = paths.current();
        open_missing_devices(&wanted, &mut devices, activity);
        if devices.is_empty() {
            std::thread::sleep(POLL_RESCAN_INTERVAL);
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
                        std::thread::sleep(POLL_RESCAN_INTERVAL);
                        continue;
                    }
                    Ok(_) => {}
                }
                polls.iter().map(|poll| poll.revents()).collect()
            };
            // Removing in reverse preserves the unprocessed readiness indices.
            for (index, ready) in ready.into_iter().enumerate().rev() {
                if !ready.intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) {
                    continue;
                }
                if matches!(
                    drain_device(&mut devices[index], stop, activity),
                    DrainOutcome::Dead
                ) {
                    activity
                        .lock()
                        .expect("human input state poisoned")
                        .disconnect(&devices[index].path);
                    devices.swap_remove(index);
                }
            }
        }
    }
}

/// Drain all currently available bytes from one device, parsing complete
/// 24-byte events. Record activity and held keys; `Dead` when
/// the fd errors or hits EOF (evdev nodes never EOF while present; dropping
/// also keeps regular-file probes from busy-spinning on constant readiness).
fn drain_device(
    device: &mut WatchedDevice,
    stop: &AtomicBool,
    activity: &Mutex<PhysicalActivity>,
) -> DrainOutcome {
    let mut buffer = [0_u8; INPUT_EVENT_SIZE * 32];
    loop {
        if stop.load(Ordering::Acquire) {
            return DrainOutcome::Quiet;
        }
        match device.file.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return DrainOutcome::Quiet;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!(
                    "computer-use-mcp: physical input device {} failed ({error}); reopening",
                    device.path.display()
                );
                return DrainOutcome::Dead;
            }
            Ok(0) => {
                if !device.carry.is_empty() {
                    eprintln!(
                        "computer-use-mcp: physical input device {} ended with an incomplete event; reopening",
                        device.path.display()
                    );
                }
                return DrainOutcome::Dead;
            }
            Ok(consumed) => {
                let mut pending = std::mem::take(&mut device.carry);
                pending.extend_from_slice(&buffer[..consumed]);
                let complete = pending.len() / INPUT_EVENT_SIZE * INPUT_EVENT_SIZE;
                let mut state = activity.lock().expect("human input state poisoned");
                let now = Instant::now();
                for chunk in pending[..complete].chunks_exact(INPUT_EVENT_SIZE) {
                    let chunk: &[u8; INPUT_EVENT_SIZE] =
                        chunk.try_into().expect("chunk holds a full event");
                    let event = parse_input_event(chunk);
                    if event.kind == EV_SYN && event.code == 3 {
                        // SYN_DROPPED means held-state history is incomplete.
                        // Reopen and query the kernel instead of leaving a lost
                        // release permanently busy or claiming the seat is idle.
                        eprintln!(
                            "computer-use-mcp: physical input events dropped; reopening device to refresh held state"
                        );
                        state.record(now);
                        return DrainOutcome::Dead;
                    }
                    if event.kind == EV_ABS
                        && device.absolute.insert(event.code, event.value) == Some(event.value)
                    {
                        continue;
                    }
                    if is_takeover_event(&event) {
                        state.event(&device.path, event, now);
                    }
                }
                pending.copy_within(complete.., 0);
                pending.truncate(pending.len() - complete);
                device.carry = pending;
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
    use std::time::Instant as WallInstant;

    use super::*;

    fn absent(_: &Path) -> bool {
        false
    }

    fn present(_: &Path) -> bool {
        true
    }

    #[test]
    fn env_flag_forces_takeover_without_filesystem() {
        assert!(CooperativeSignal::resolve(Some("1".into()), None, None,).requested(absent));
        assert!(CooperativeSignal::resolve(Some("yes".into()), None, None,).requested(absent));
        assert!(!CooperativeSignal::resolve(Some("0".into()), None, None,).requested(present));
        assert!(!CooperativeSignal::resolve(None, None, None).requested(present));
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
        assert!(
            CooperativeSignal::resolve(None, None, Some("/run/user/1".into()),).requested(present)
        );
        assert!(
            !CooperativeSignal::resolve(None, None, Some("/run/user/1".into()),).requested(absent)
        );
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

    #[tokio::test(start_paused = true)]
    async fn physical_activity_clears_after_a_full_minute_without_reset() {
        let monitor = TakeoverMonitor::new();
        assert!(
            !takeover_requested(),
            "test environment must not assert a takeover signal"
        );
        assert!(!monitor.is_active());
        monitor.trip();
        assert!(monitor.is_active());
        let paused_generation = monitor.status().generation;
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(monitor.is_active());
        assert_eq!(monitor.status().remaining, Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!monitor.is_active());
        let idle_generation = monitor.status().generation;
        assert_ne!(
            idle_generation, paused_generation,
            "pause observations become stale"
        );
        assert_eq!(
            monitor.status().generation,
            idle_generation,
            "polling idle does not stale fresh observations"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn further_motion_restarts_quiet_period_and_held_input_requires_release() {
        let mut activity = PhysicalActivity::default();
        let path = Path::new("synthetic-keyboard");
        activity.connect(path, HashSet::new());
        activity.event(
            path,
            parse_input_event(&encode_event(EV_KEY, 30, 1)),
            Instant::now(),
        );
        assert!(activity.status(false, Instant::now()).busy());
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(
            activity.status(false, Instant::now()).state,
            HumanInputState::PhysicalInputHeld
        );
        activity.event(
            path,
            parse_input_event(&encode_event(EV_KEY, 30, 0)),
            Instant::now(),
        );
        tokio::time::advance(Duration::from_secs(59)).await;
        activity.event(
            path,
            parse_input_event(&encode_event(EV_REL, 0, 3)),
            Instant::now(),
        );
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(activity.status(false, Instant::now()).busy());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!activity.status(false, Instant::now()).busy());
    }

    #[tokio::test(start_paused = true)]
    async fn cooperative_handoff_blocks_until_cleared_then_waits_a_minute() {
        let monitor = TakeoverMonitor::new();
        monitor.manual.store(true, Ordering::Release);
        assert_eq!(monitor.status().state, HumanInputState::HandoffRequested);
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(monitor.is_active());
        monitor.manual.store(false, Ordering::Release);
        assert_eq!(monitor.status().remaining, HUMAN_IDLE_INTERVAL);
        tokio::time::advance(HUMAN_IDLE_INTERVAL).await;
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
        assert_eq!(moved.kind, EV_REL);
        assert_eq!(moved.value, -7);
    }

    #[test]
    fn takeover_filter_triggers_only_on_key_rel_abs() {
        // Presses, repeats, motion, and absolute positioning indicate activity:
        // agent EIS input never reaches /dev/input, so any of these is human.
        for (kind, code, value) in [
            (EV_KEY, 30, 1),
            (EV_KEY, 30, 2),
            (EV_KEY, 30, 0),
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
            (EV_KEY, 30, -1),
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
            device_paths_from_override(Some("/dev/input/event0:/tmp/a:: /tmp/b :/tmp/a".into())),
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
            vec![PathBuf::from("/dev/null")],
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
        let start = WallInstant::now();
        while start.elapsed() < bound {
            if monitor.is_active() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        monitor.is_active()
    }

    #[test]
    fn watcher_records_activity_from_prefilled_file() {
        let path = unique_scratch("press");
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_event(EV_SYN, 0, 0));
        payload.extend_from_slice(&encode_event(EV_KEY, 30, 1));
        std::fs::write(&path, &payload).expect("write synthetic events");
        let monitor = TakeoverMonitor::new();
        assert!(!monitor.is_active());
        let _watch = spawn_hardware_watcher(
            Arc::clone(&monitor.activity),
            DevicePaths::Fixed(vec![path.clone()]),
        );
        assert!(
            wait_for_active(&monitor, Duration::from_secs(5)),
            "prefilled EV_KEY bytes must mark physical input busy"
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
        let _watch = spawn_hardware_watcher(
            Arc::clone(&monitor.activity),
            DevicePaths::Fixed(vec![path.clone()]),
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !monitor.is_active(),
            "EV_SYN traffic must not trip the hardware latch"
        );
        let _ = std::fs::remove_file(&path);

        // An empty device set is fail-open disabled, never latched.
        let idle = TakeoverMonitor::new();
        let _idle_watch =
            spawn_hardware_watcher(Arc::clone(&idle.activity), DevicePaths::Fixed(Vec::new()));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!idle.is_active());
    }

    #[test]
    fn watcher_continues_after_activity_and_shutdown_joins_it() {
        use std::io::Write;
        let path = unique_scratch("continuous-fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &path, rustix::fs::Mode::RWXU).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let monitor = TakeoverMonitor::new();
        *monitor.watcher.lock().unwrap() = spawn_hardware_watcher(
            Arc::clone(&monitor.activity),
            DevicePaths::Fixed(vec![path.clone()]),
        );
        assert!(monitor.status().available);
        writer.write_all(&encode_event(EV_KEY, 30, 1)).unwrap();
        assert!(wait_for_active(&monitor, Duration::from_secs(1)));
        assert_eq!(monitor.status().state, HumanInputState::PhysicalInputHeld);
        let generation = monitor.status().generation;
        writer.write_all(&encode_event(EV_KEY, 30, 0)).unwrap();
        let start = WallInstant::now();
        while monitor.status().generation == generation && start.elapsed() < Duration::from_secs(1)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            monitor.status().generation != generation,
            "watcher must continue tracking after interruption"
        );
        assert_eq!(monitor.status().state, HumanInputState::QuietPeriod);
        monitor.stop();
        assert!(!monitor.status().available);
        assert!(monitor.watcher.lock().unwrap().is_none());
        drop(writer);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn event_batches_preserve_partial_records_and_ignore_repeated_absolute_positions() {
        use std::io::Write;
        let path = unique_scratch("partial-fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &path, rustix::fs::Mode::RWXU).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let activity = Mutex::new(PhysicalActivity::default());
        let mut devices = Vec::new();
        open_missing_devices(&[path.clone(), path.clone()], &mut devices, &activity);
        assert_eq!(devices.len(), 1, "duplicate paths share one registration");
        let device = &mut devices[0];
        let press = encode_event(EV_KEY, 30, 1);
        writer.write_all(&press[..11]).unwrap();
        assert!(matches!(
            drain_device(device, &AtomicBool::new(false), &activity),
            DrainOutcome::Quiet
        ));
        assert!(
            !activity
                .lock()
                .unwrap()
                .status(false, Instant::now())
                .busy()
        );

        let mut batch = press[11..].to_vec();
        batch.extend_from_slice(&encode_event(EV_KEY, 30, 0));
        batch.extend_from_slice(&encode_event(EV_ABS, 0, 42));
        writer.write_all(&batch).unwrap();
        assert!(matches!(
            drain_device(device, &AtomicBool::new(false), &activity),
            DrainOutcome::Quiet
        ));
        let status = activity.lock().unwrap().status(false, Instant::now());
        assert_eq!(
            status.state,
            HumanInputState::QuietPeriod,
            "release in the same batch clears held state"
        );
        assert!(device.carry.is_empty());

        writer.write_all(&encode_event(EV_ABS, 0, 42)).unwrap();
        assert!(matches!(
            drain_device(device, &AtomicBool::new(false), &activity),
            DrainOutcome::Quiet
        ));
        assert_eq!(
            activity
                .lock()
                .unwrap()
                .status(false, Instant::now())
                .generation,
            status.generation
        );
        drop(devices);
        drop(writer);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_held_keys_and_dropped_event_history_require_resynchronization() {
        let path = unique_scratch("dropped-history");
        std::fs::write(&path, encode_event(EV_SYN, 3, 0)).unwrap();
        let (mut device, mut held) = WatchedDevice::open(&path).unwrap();
        // Seed the kernel-query result at the device boundary, including a key
        // pressed before monitoring began.
        held.insert(30);
        let monitor = TakeoverMonitor::new();
        monitor.activity.lock().unwrap().connect(&path, held);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(monitor.status().state, HumanInputState::PhysicalInputHeld);
        assert!(matches!(
            drain_device(&mut device, &AtomicBool::new(false), &monitor.activity),
            DrainOutcome::Dead
        ));
        monitor.activity.lock().unwrap().disconnect(&path);
        drop(device);
        let (reopened, held) = WatchedDevice::open(&path).unwrap();
        monitor.activity.lock().unwrap().connect(&path, held);
        assert_eq!(monitor.status().state, HumanInputState::QuietPeriod);
        tokio::time::advance(HUMAN_IDLE_INTERVAL).await;
        assert!(!monitor.status().busy());
        assert!(monitor.status().available);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
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
        let watch = spawn_hardware_watcher(
            Arc::clone(&monitor.activity),
            DevicePaths::Fixed(vec![path.clone()]),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(70));
        std::fs::write(&path, encode_event(EV_KEY, 30, 1)).unwrap();
        assert!(wait_for_active(&monitor, Duration::from_secs(1)));
        drop(watch);
        std::fs::remove_file(path).unwrap();

        let watch = spawn_hardware_watcher(
            Arc::new(Mutex::new(PhysicalActivity::default())),
            DevicePaths::Fixed(Vec::new()),
        )
        .unwrap();
        let stopped = Arc::clone(&watch.stop);
        let before = WallInstant::now();
        drop(watch);
        assert!(stopped.load(Ordering::Acquire));
        assert!(before.elapsed() < Duration::from_secs(1));
        assert_eq!(Arc::strong_count(&stopped), 1, "worker thread has exited");
    }
}
