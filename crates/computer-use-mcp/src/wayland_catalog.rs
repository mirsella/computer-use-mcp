//! Pure reducers and the persistent Wayland catalog connection.
//!
//! The reducers intentionally do not depend on a live compositor.  Protocol
//! dispatch translates generated Wayland events into these events, while the
//! runtime consumes complete snapshots published by the dedicated blocking
//! thread.  This keeps lifecycle, duplicate identity, and activation evidence
//! deterministic in unit tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tokio::sync::oneshot;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    protocol::{wl_callback, wl_registry},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{Event as ForeignHandleEvent, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{Event as ForeignListEvent, ExtForeignToplevelListV1},
};
use wayland_protocols_plasma::plasma_window_management::client::{
    org_kde_plasma_window::{Event as KdeWindowEvent, OrgKdePlasmaWindow},
    org_kde_plasma_window_management::{Event as KdeManagementEvent, OrgKdePlasmaWindowManagement},
};

use crate::window_backend::{
    BackendError, BackendKind, BackendStatus, BackendWindow, WindowCapabilities, WindowGeometry,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReducerError {
    EmptyIdentity,
    InvalidIdentity(String),
    DuplicateIdentity(String),
    InvalidState { uuid: String, flags: u32 },
}

impl fmt::Display for ReducerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentity => formatter.write_str("protocol record has an empty identity"),
            Self::InvalidIdentity(identity) => {
                write!(
                    formatter,
                    "protocol record has an invalid identity {identity:?}"
                )
            }
            Self::DuplicateIdentity(identity) => {
                write!(
                    formatter,
                    "protocol emitted duplicate identity {identity:?}"
                )
            }
            Self::InvalidState { uuid, flags } => {
                write!(
                    formatter,
                    "protocol emitted unsupported state flags {flags:#x} for {uuid:?}"
                )
            }
        }
    }
}

impl std::error::Error for ReducerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignEvent {
    Created {
        handle: String,
    },
    Title {
        handle: String,
        title: String,
    },
    AppId {
        handle: String,
        app_id: String,
    },
    Identifier {
        handle: String,
        identifier: String,
    },
    State {
        handle: String,
        states: BTreeSet<String>,
    },
    OutputEnter {
        handle: String,
        output: String,
    },
    OutputLeave {
        handle: String,
        output: String,
    },
    Done {
        handle: String,
    },
    Closed {
        handle: String,
    },
    Finished,
    Reset,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ForeignProperties {
    title: String,
    app_id: Option<String>,
    identifier: Option<String>,
    states: BTreeSet<String>,
    outputs: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
struct ForeignRecord {
    pending: ForeignProperties,
    committed: Option<ForeignProperties>,
}

#[derive(Debug, Default)]
pub struct ForeignToplevelReducer {
    records: BTreeMap<String, ForeignRecord>,
    finished: bool,
}

impl ForeignToplevelReducer {
    pub fn apply(&mut self, event: ForeignEvent) -> Result<(), ReducerError> {
        match event {
            ForeignEvent::Created { handle } => {
                if handle.is_empty() {
                    return Err(ReducerError::EmptyIdentity);
                }
                if self.records.contains_key(&handle) {
                    return Err(ReducerError::DuplicateIdentity(handle));
                }
                self.finished = false;
                self.records.insert(handle, ForeignRecord::default());
            }
            ForeignEvent::Title { handle, title } => {
                self.record_mut(&handle)?.pending.title = title
            }
            ForeignEvent::AppId { handle, app_id } => {
                self.record_mut(&handle)?.pending.app_id = nonempty(app_id)
            }
            ForeignEvent::Identifier { handle, identifier } => {
                if !valid_foreign_identifier(&identifier) {
                    return Err(ReducerError::InvalidIdentity(identifier));
                }
                let record = self.record_mut(&handle)?;
                record.pending.identifier = Some(identifier);
            }
            ForeignEvent::State { handle, states } => {
                self.record_mut(&handle)?.pending.states = states;
            }
            ForeignEvent::OutputEnter { handle, output } => {
                self.record_mut(&handle)?.pending.outputs.insert(output);
            }
            ForeignEvent::OutputLeave { handle, output } => {
                self.record_mut(&handle)?.pending.outputs.remove(&output);
            }
            ForeignEvent::Done { handle } => {
                let record = self.record_mut(&handle)?;
                record.committed = Some(record.pending.clone());
            }
            ForeignEvent::Closed { handle } => {
                self.records.remove(&handle);
            }
            ForeignEvent::Finished => {
                self.records.clear();
                self.finished = true;
            }
            ForeignEvent::Reset => {
                self.records.clear();
                self.finished = false;
            }
        }
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn snapshot(&self) -> Result<Vec<BackendWindow>, ReducerError> {
        let mut identities = BTreeSet::new();
        let mut result = Vec::new();
        for record in self.records.values() {
            let Some(properties) = record.committed.as_ref() else {
                continue;
            };
            let Some(identifier) = properties.identifier.as_deref() else {
                continue;
            };
            if !identities.insert(identifier.to_owned()) {
                return Err(ReducerError::DuplicateIdentity(identifier.to_owned()));
            }
            result.push(BackendWindow {
                backend_identity: identifier.to_owned(),
                // The standard protocol has no PID or process identity. An
                // app_id is descriptive only, so keep each window separate
                // rather than pretending two launches are one application.
                application_identity: format!("window:{identifier}"),
                title: properties.title.clone(),
                app_id: properties.app_id.clone(),
                pid: None,
                states: properties.states.clone(),
                outputs: properties.outputs.iter().cloned().collect(),
                virtual_desktops: Vec::new(),
                resource_name: None,
                geometry: None,
                source: BackendKind::ForeignToplevel,
                capabilities: WindowCapabilities::foreign_toplevel(),
                atspi: None,
            });
        }
        Ok(result)
    }

    fn record_mut(&mut self, handle: &str) -> Result<&mut ForeignRecord, ReducerError> {
        self.records
            .get_mut(handle)
            .ok_or(ReducerError::EmptyIdentity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KdeEvent {
    Created {
        uuid: String,
    },
    Title {
        uuid: String,
        title: String,
    },
    AppId {
        uuid: String,
        app_id: String,
    },
    Pid {
        uuid: String,
        pid: u32,
    },
    ResourceName {
        uuid: String,
        resource_name: String,
    },
    Geometry {
        uuid: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        client: bool,
    },
    State {
        uuid: String,
        flags: u32,
    },
    VirtualDesktopEntered {
        uuid: String,
        id: String,
    },
    VirtualDesktopLeft {
        uuid: String,
        id: String,
    },
    InitialState {
        uuid: String,
    },
    Unmapped {
        uuid: String,
    },
    Reset,
}

#[derive(Debug, Clone, Default)]
struct KdeRecord {
    title: String,
    app_id: Option<String>,
    pid: Option<u32>,
    resource_name: Option<String>,
    geometry: Option<WindowGeometry>,
    flags: u32,
    unknown_flags: u32,
    virtual_desktops: BTreeSet<String>,
    initialized: bool,
    unmapped: bool,
}

#[derive(Debug)]
pub struct KdeRichReducer {
    records: BTreeMap<String, KdeRecord>,
}

impl KdeRichReducer {
    pub const MIN_VERSION: u32 = 17;

    pub fn new(protocol_version: u32) -> Result<Self, BackendError> {
        if protocol_version < Self::MIN_VERSION {
            return Err(BackendError::Unsupported(format!(
                "KDE Plasma window management version {protocol_version} is below required version {}",
                Self::MIN_VERSION
            )));
        }
        Ok(Self {
            records: BTreeMap::new(),
        })
    }

    pub fn apply(&mut self, event: KdeEvent) -> Result<(), ReducerError> {
        match event {
            KdeEvent::Created { uuid } => {
                if uuid.is_empty() {
                    return Err(ReducerError::EmptyIdentity);
                }
                if self
                    .records
                    .insert(uuid.clone(), KdeRecord::default())
                    .is_some()
                {
                    return Err(ReducerError::DuplicateIdentity(uuid));
                }
            }
            KdeEvent::Title { uuid, title } => self.record_mut(&uuid)?.title = title,
            KdeEvent::AppId { uuid, app_id } => self.record_mut(&uuid)?.app_id = nonempty(app_id),
            KdeEvent::Pid { uuid, pid } => self.record_mut(&uuid)?.pid = Some(pid),
            KdeEvent::ResourceName {
                uuid,
                resource_name,
            } => self.record_mut(&uuid)?.resource_name = nonempty(resource_name),
            KdeEvent::Geometry {
                uuid,
                x,
                y,
                width,
                height,
                client,
            } => {
                self.record_mut(&uuid)?.geometry = Some(WindowGeometry {
                    x,
                    y,
                    width,
                    height,
                    client,
                });
            }
            KdeEvent::State { uuid, flags } => {
                let known_flags = flags & KDE_KNOWN_FLAGS;
                let unknown_flags = flags & !KDE_KNOWN_FLAGS;
                if unknown_flags != 0 {
                    eprintln!(
                        "computer-use-mcp: preserving unknown KDE state bits {unknown_flags:#x} for UUID {uuid:?}"
                    );
                }
                let record = self.record_mut(&uuid)?;
                record.flags = known_flags;
                record.unknown_flags = unknown_flags;
            }
            KdeEvent::VirtualDesktopEntered { uuid, id } => {
                self.record_mut(&uuid)?.virtual_desktops.insert(id);
            }
            KdeEvent::VirtualDesktopLeft { uuid, id } => {
                self.record_mut(&uuid)?.virtual_desktops.remove(&id);
            }
            KdeEvent::InitialState { uuid } => self.record_mut(&uuid)?.initialized = true,
            KdeEvent::Unmapped { uuid } => {
                let record = self.record_mut(&uuid)?;
                // KWin may send unmapped before the initial_state event. Keep
                // the tombstone until the proxy lifecycle ends so a later
                // initial_state is harmless rather than a backend protocol
                // failure.
                record.unmapped = true;
            }
            KdeEvent::Reset => self.records.clear(),
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<BackendWindow> {
        self.records
            .iter()
            .filter(|(_, record)| record.initialized && !record.unmapped)
            .map(|(uuid, record)| BackendWindow {
                backend_identity: uuid.clone(),
                // KDE supplies a PID for initialized windows. Use that exact
                // process identity for grouping multiple windows; app_id is
                // descriptive and must not be used as a process join key.
                application_identity: record
                    .pid
                    .map_or_else(|| format!("uuid:{uuid}"), |pid| format!("pid:{pid}")),
                title: record.title.clone(),
                app_id: record.app_id.clone(),
                pid: record.pid,
                states: kde_states(record.flags, record.unknown_flags),
                outputs: Vec::new(),
                virtual_desktops: record.virtual_desktops.iter().cloned().collect(),
                resource_name: record.resource_name.clone(),
                geometry: record.geometry,
                source: BackendKind::KdePlasma,
                capabilities: WindowCapabilities::kde_rich(),
                atspi: None,
            })
            .collect()
    }

    fn is_unmapped(&self, uuid: &str) -> bool {
        self.records.get(uuid).is_some_and(|record| record.unmapped)
    }

    fn is_initialized(&self, uuid: &str) -> bool {
        self.records
            .get(uuid)
            .is_some_and(|record| record.initialized)
    }

    fn remap(&mut self, uuid: String) -> Result<(), ReducerError> {
        if !self.is_unmapped(&uuid) {
            return Err(ReducerError::DuplicateIdentity(uuid));
        }
        self.records.remove(&uuid);
        self.apply(KdeEvent::Created { uuid })
    }

    fn terminal(&mut self, uuid: &str) {
        self.records.remove(uuid);
    }

    pub fn is_active(&self, uuid: &str) -> Result<bool, ReducerError> {
        let record = self.records.get(uuid).ok_or(ReducerError::EmptyIdentity)?;
        Ok(record.flags & KDE_STATE_ACTIVE != 0)
    }

    fn record_mut(&mut self, uuid: &str) -> Result<&mut KdeRecord, ReducerError> {
        self.records
            .get_mut(uuid)
            .ok_or(ReducerError::EmptyIdentity)
    }
}

pub const KDE_STATE_ACTIVE: u32 = 0x1;
const KDE_STATE_MINIMIZED: u32 = 0x2;
const KDE_STATE_MAXIMIZED: u32 = 0x4;
const KDE_STATE_FULLSCREEN: u32 = 0x8;
const KDE_STATE_KEEP_ABOVE: u32 = 0x10;
const KDE_STATE_KEEP_BELOW: u32 = 0x20;
const KDE_STATE_ALL_DESKTOPS: u32 = 0x40;
const KDE_STATE_DEMANDS_ATTENTION: u32 = 0x80;
const KDE_STATE_CLOSEABLE: u32 = 0x100;
const KDE_STATE_MINIMIZABLE: u32 = 0x200;
const KDE_STATE_MAXIMIZABLE: u32 = 0x400;
const KDE_STATE_FULLSCREENABLE: u32 = 0x800;
const KDE_STATE_SKIP_TASKBAR: u32 = 0x1000;
const KDE_STATE_SHADEABLE: u32 = 0x2000;
const KDE_STATE_SHADED: u32 = 0x4000;
const KDE_STATE_MOVABLE: u32 = 0x8000;
const KDE_STATE_RESIZABLE: u32 = 0x10000;
const KDE_STATE_VIRTUAL_DESKTOP_CHANGEABLE: u32 = 0x20000;
const KDE_STATE_SKIP_SWITCHER: u32 = 0x40000;
const KDE_KNOWN_FLAGS: u32 = KDE_STATE_ACTIVE
    | KDE_STATE_MINIMIZED
    | KDE_STATE_MAXIMIZED
    | KDE_STATE_FULLSCREEN
    | KDE_STATE_KEEP_ABOVE
    | KDE_STATE_KEEP_BELOW
    | KDE_STATE_ALL_DESKTOPS
    | KDE_STATE_DEMANDS_ATTENTION
    | KDE_STATE_CLOSEABLE
    | KDE_STATE_MINIMIZABLE
    | KDE_STATE_MAXIMIZABLE
    | KDE_STATE_FULLSCREENABLE
    | KDE_STATE_SKIP_TASKBAR
    | KDE_STATE_SHADEABLE
    | KDE_STATE_SHADED
    | KDE_STATE_MOVABLE
    | KDE_STATE_RESIZABLE
    | KDE_STATE_VIRTUAL_DESKTOP_CHANGEABLE
    | KDE_STATE_SKIP_SWITCHER;

fn kde_states(flags: u32, unknown_flags: u32) -> BTreeSet<String> {
    [
        (KDE_STATE_ACTIVE, "active"),
        (KDE_STATE_MINIMIZED, "minimized"),
        (KDE_STATE_MAXIMIZED, "maximized"),
        (KDE_STATE_FULLSCREEN, "fullscreen"),
        (KDE_STATE_KEEP_ABOVE, "keep_above"),
        (KDE_STATE_KEEP_BELOW, "keep_below"),
        (KDE_STATE_ALL_DESKTOPS, "on_all_desktops"),
        (KDE_STATE_DEMANDS_ATTENTION, "demands_attention"),
        (KDE_STATE_CLOSEABLE, "closeable"),
        (KDE_STATE_MINIMIZABLE, "minimizable"),
        (KDE_STATE_MAXIMIZABLE, "maximizable"),
        (KDE_STATE_FULLSCREENABLE, "fullscreenable"),
        (KDE_STATE_SKIP_TASKBAR, "skiptaskbar"),
        (KDE_STATE_SHADEABLE, "shadeable"),
        (KDE_STATE_SHADED, "shaded"),
        (KDE_STATE_MOVABLE, "movable"),
        (KDE_STATE_RESIZABLE, "resizable"),
        (
            KDE_STATE_VIRTUAL_DESKTOP_CHANGEABLE,
            "virtual_desktop_changeable",
        ),
        (KDE_STATE_SKIP_SWITCHER, "skipswitcher"),
    ]
    .into_iter()
    .filter(|(bit, _)| flags & bit != 0)
    .map(|(_, name)| name.to_owned())
    .chain((unknown_flags != 0).then(|| format!("unknown_state_bits_{unknown_flags:#x}")))
    .collect()
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn valid_foreign_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationTracker {
    pub uuid: String,
    pub deadline: Instant,
}

impl ActivationTracker {
    pub fn new(uuid: impl Into<String>, now: Instant, timeout: Duration) -> Self {
        Self {
            uuid: uuid.into(),
            deadline: now + timeout,
        }
    }

    pub fn verify(&self, uuid: &str, active: bool) -> bool {
        self.verify_at(uuid, active, Instant::now())
    }

    pub fn verify_at(&self, uuid: &str, active: bool, observed_at: Instant) -> bool {
        self.uuid == uuid && active && observed_at < self.deadline
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// A bounded, command-driven façade around the blocking Wayland event thread.
/// The generated protocol dispatch code is kept below this reducer boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositorSnapshot {
    pub records: Vec<BackendWindow>,
    pub standard: BackendStatus,
    pub kde: BackendStatus,
}

struct ProtocolRuntime {
    connection: Connection,
    queue: EventQueue<ProtocolState>,
    state: ProtocolState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdvertisedGlobal {
    interface: String,
    version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolBackend {
    Standard,
    Kde,
}

struct ProtocolState {
    backend: ProtocolBackend,
    registry: Option<wl_registry::WlRegistry>,
    globals: BTreeMap<u32, AdvertisedGlobal>,
    foreign: ForeignToplevelReducer,
    foreign_handles: BTreeMap<u32, String>,
    foreign_proxies: BTreeMap<u32, ExtForeignToplevelHandleV1>,
    foreign_list: Option<ExtForeignToplevelListV1>,
    kde: Option<KdeRichReducer>,
    kde_windows: BTreeMap<u32, String>,
    kde_proxies: BTreeMap<u32, OrgKdePlasmaWindow>,
    kde_unmapped: BTreeSet<u32>,
    kde_manager: Option<OrgKdePlasmaWindowManagement>,
    snapshot: CompositorSnapshot,
    activation: Option<ActivationWait>,
    initial_sync: Option<wl_callback::WlCallback>,
    initial_done: bool,
}

struct ActivationWait {
    tracker: ActivationTracker,
    reply: oneshot::Sender<Result<(), BackendError>>,
    dispatched: bool,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl ProtocolState {
    fn new(backend: ProtocolBackend) -> Self {
        Self {
            backend,
            registry: None,
            globals: BTreeMap::new(),
            foreign: ForeignToplevelReducer::default(),
            foreign_handles: BTreeMap::new(),
            foreign_proxies: BTreeMap::new(),
            foreign_list: None,
            kde: None,
            kde_windows: BTreeMap::new(),
            kde_proxies: BTreeMap::new(),
            kde_unmapped: BTreeSet::new(),
            kde_manager: None,
            snapshot: CompositorSnapshot {
                records: Vec::new(),
                standard: match backend {
                    ProtocolBackend::Standard => BackendStatus::Unavailable(
                        "Wayland standard backend is initializing".into(),
                    ),
                    ProtocolBackend::Kde => BackendStatus::Unsupported(
                        "standard backend uses a separate connection".into(),
                    ),
                },
                kde: match backend {
                    ProtocolBackend::Kde => {
                        BackendStatus::Unavailable("Wayland KDE backend is initializing".into())
                    }
                    ProtocolBackend::Standard => {
                        BackendStatus::Unsupported("KDE backend uses a separate connection".into())
                    }
                },
            },
            activation: None,
            initial_sync: None,
            initial_done: false,
        }
    }

    fn publish(&mut self) {
        let standard_records = match self.foreign.snapshot() {
            Ok(records) => records,
            Err(error) => {
                eprintln!("computer-use-mcp: standard foreign-toplevel reducer failed: {error}");
                self.snapshot.standard = BackendStatus::Unavailable(error.to_string());
                Vec::new()
            }
        };
        let records = if self.snapshot.kde == BackendStatus::Supported {
            // KDE's UUID-backed stream is the authoritative compositor source
            // when it is enabled. Do not merge it with standard records by
            // title, PID, app ID, or geometry; the protocols expose no exact
            // cross-protocol identity relation.
            self.kde
                .as_ref()
                .map_or_else(Vec::new, KdeRichReducer::snapshot)
        } else {
            standard_records
        };
        self.snapshot.records = records;
    }

    fn fail_standard(&mut self, reason: impl Into<String>) {
        self.snapshot.standard = BackendStatus::Unavailable(reason.into());
        if let Err(error) = self.foreign.apply(ForeignEvent::Reset) {
            eprintln!(
                "computer-use-mcp: failed to reset standard foreign-toplevel reducer: {error}"
            );
        }
        destroy_foreign_resources(self);
        self.publish();
    }

    fn fail_kde(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.snapshot.kde = BackendStatus::Unavailable(reason.clone());
        if let Some(activation) = self.activation.take() {
            let _ = activation.reply.send(Err(BackendError::Unknown(format!(
                "KDE activation outcome is unknown after backend failure: {reason}"
            ))));
        }
        self.kde = None;
        destroy_kde_resources(self);
        self.publish();
    }

    fn fail_active(&mut self, reason: impl Into<String>) {
        match self.backend {
            ProtocolBackend::Standard => self.fail_standard(reason),
            ProtocolBackend::Kde => self.fail_kde(reason),
        }
    }

    fn shutdown_resources(&mut self) {
        if let Some(activation) = self.activation.take() {
            let _ = activation.reply.send(Err(BackendError::Unknown(
                "KDE activation was interrupted by backend shutdown".into(),
            )));
        }
        destroy_foreign_resources(self);
        destroy_kde_resources(self);
    }
}

fn destroy_foreign_resources(state: &mut ProtocolState) {
    // ext-foreign-toplevel-list-v1 requires child handles to be destroyed
    // before the manager list. Keep this order on both normal Finished and
    // fail-closed cleanup paths.
    for proxy in state.foreign_proxies.values() {
        proxy.destroy();
    }
    state.foreign_proxies.clear();
    state.foreign_handles.clear();
    if let Some(list) = state.foreign_list.take() {
        list.destroy();
    }
}

fn destroy_kde_resources(state: &mut ProtocolState) {
    // org_kde_plasma_window_management owns the child window resources. The
    // child destructors must be sent before the manager is dropped when the
    // connection is being torn down or the global is withdrawn; the manager
    // interface exposes no destructor request of its own.
    if state.kde_windows.len() != state.kde_proxies.len() {
        eprintln!(
            "computer-use-mcp: KDE cleanup invariant mismatch: {} window IDs, {} proxies",
            state.kde_windows.len(),
            state.kde_proxies.len()
        );
    }
    for proxy in state.kde_proxies.values() {
        proxy.destroy();
    }
    state.kde_proxies.clear();
    state.kde_windows.clear();
    state.kde_unmapped.clear();
    // The Plasma management interface exposes no manager destructor request;
    // dropping it after the child resources is the only legal cleanup path.
    let _ = state.kde_manager.take();
}

impl Dispatch<wl_callback::WlCallback, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.initial_done = true;
            state.initial_sync = None;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state
                    .globals
                    .insert(name, AdvertisedGlobal { interface, version });
            }
            wl_registry::Event::GlobalRemove { name } => {
                let Some(removed) = state.globals.remove(&name) else {
                    eprintln!("computer-use-mcp: Wayland registry removed unknown global {name}");
                    return;
                };
                if removed.interface == "ext_foreign_toplevel_list_v1"
                    && state.foreign_list.is_some()
                {
                    state.foreign_list = None;
                    state.fail_standard("foreign-toplevel global was removed");
                }
                if removed.interface == "org_kde_plasma_window_management"
                    && state.kde_manager.is_some()
                {
                    state.fail_kde("KDE Plasma window-management global was removed");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ForeignListEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ForeignListEvent::Toplevel { toplevel } => {
                let id = toplevel.id().protocol_id();
                let handle = format!("handle-{id}");
                state.foreign_handles.insert(id, handle.clone());
                state.foreign_proxies.insert(id, toplevel);
                if let Err(error) = state.foreign.apply(ForeignEvent::Created { handle }) {
                    state.fail_standard(format!("foreign-toplevel lifecycle failed: {error}"));
                }
            }
            ForeignListEvent::Finished => {
                if let Err(error) = state.foreign.apply(ForeignEvent::Finished) {
                    state.fail_standard(format!(
                        "foreign-toplevel finished lifecycle failed: {error}"
                    ));
                }
                destroy_foreign_resources(state);
                state.snapshot.standard = BackendStatus::Unavailable(
                    "foreign-toplevel manager finished and was destroyed".into(),
                );
            }
            _ => {}
        }
        state.publish();
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ForeignHandleEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        let Some(handle) = state.foreign_handles.get(&id).cloned() else {
            eprintln!("computer-use-mcp: foreign-toplevel event arrived for unknown handle {id}");
            return;
        };
        let translated = match event {
            ForeignHandleEvent::Title { title } => ForeignEvent::Title { handle, title },
            ForeignHandleEvent::AppId { app_id } => ForeignEvent::AppId { handle, app_id },
            ForeignHandleEvent::Identifier { identifier } => {
                if !valid_foreign_identifier(&identifier) {
                    eprintln!(
                        "computer-use-mcp: ignoring foreign-toplevel record with invalid identifier: handle={handle:?} bytes={}",
                        identifier.len()
                    );
                    proxy.destroy();
                    state.foreign_handles.remove(&id);
                    state.foreign_proxies.remove(&id);
                    if let Err(error) = state.foreign.apply(ForeignEvent::Closed {
                        handle: handle.clone(),
                    }) {
                        eprintln!(
                            "computer-use-mcp: invalid foreign-toplevel record cleanup failed: {error}"
                        );
                    }
                    state.publish();
                    return;
                }
                ForeignEvent::Identifier { handle, identifier }
            }
            ForeignHandleEvent::Done => ForeignEvent::Done { handle },
            ForeignHandleEvent::Closed => {
                let event = ForeignEvent::Closed { handle };
                proxy.destroy();
                state.foreign_handles.remove(&id);
                state.foreign_proxies.remove(&id);
                event
            }
            _ => return,
        };
        if let Err(error) = state.foreign.apply(translated) {
            state.fail_standard(format!("foreign-toplevel lifecycle failed: {error}"));
        }
        state.publish();
    }
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &OrgKdePlasmaWindowManagement,
        event: KdeManagementEvent,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let KdeManagementEvent::WindowWithUuid { uuid, .. } = event {
            let window = proxy.get_window_by_uuid(uuid.clone(), qh, uuid.clone());
            let protocol_id = window.id().protocol_id();
            let duplicate_protocol_id = state.kde_windows.contains_key(&protocol_id);
            let duplicate_active_uuid = state
                .kde_windows
                .iter()
                .any(|(id, mapped_uuid)| mapped_uuid == &uuid && !state.kde_unmapped.contains(id));
            if duplicate_protocol_id || duplicate_active_uuid {
                eprintln!(
                    "computer-use-mcp: refusing duplicate KDE window identity: protocol_id={protocol_id} uuid={uuid:?}"
                );
                window.destroy();
                state.fail_kde("KDE window lifecycle emitted a duplicate active identity");
                return;
            }
            state.kde_windows.insert(protocol_id, uuid.clone());
            state.kde_proxies.insert(protocol_id, window);
            let kde_error = state.kde.as_mut().and_then(|kde| {
                let result = if kde.is_unmapped(&uuid) {
                    kde.remap(uuid)
                } else {
                    kde.apply(KdeEvent::Created { uuid })
                };
                result.err()
            });
            if let Some(error) = kde_error {
                state.fail_kde(format!("KDE window lifecycle failed: {error}"));
            }
        }
        state.publish();
    }
}

impl Dispatch<OrgKdePlasmaWindow, String> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &OrgKdePlasmaWindow,
        event: KdeWindowEvent,
        uuid: &String,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let protocol_id = proxy.id().protocol_id();
        if state.kde_unmapped.contains(&protocol_id) {
            if matches!(event, KdeWindowEvent::InitialState) {
                finish_kde_tombstone(state, protocol_id, uuid);
            } else {
                eprintln!(
                    "computer-use-mcp: ignoring KDE event after unmapped before initial_state for UUID {uuid:?}"
                );
            }
            state.publish();
            return;
        }
        let observed_at = Instant::now();
        let event = match event {
            KdeWindowEvent::TitleChanged { title } => KdeEvent::Title {
                uuid: uuid.clone(),
                title,
            },
            KdeWindowEvent::AppIdChanged { app_id } => KdeEvent::AppId {
                uuid: uuid.clone(),
                app_id,
            },
            KdeWindowEvent::PidChanged { pid } => KdeEvent::Pid {
                uuid: uuid.clone(),
                pid,
            },
            KdeWindowEvent::ResourceNameChanged { resource_name } => KdeEvent::ResourceName {
                uuid: uuid.clone(),
                resource_name,
            },
            KdeWindowEvent::Geometry {
                x,
                y,
                width,
                height,
            } => KdeEvent::Geometry {
                uuid: uuid.clone(),
                x,
                y,
                width,
                height,
                client: false,
            },
            KdeWindowEvent::ClientGeometry {
                x,
                y,
                width,
                height,
            } => KdeEvent::Geometry {
                uuid: uuid.clone(),
                x,
                y,
                width,
                height,
                client: true,
            },
            KdeWindowEvent::StateChanged { flags } => KdeEvent::State {
                uuid: uuid.clone(),
                flags,
            },
            KdeWindowEvent::VirtualDesktopEntered { id } => KdeEvent::VirtualDesktopEntered {
                uuid: uuid.clone(),
                id,
            },
            KdeWindowEvent::VirtualDesktopLeft { is } => KdeEvent::VirtualDesktopLeft {
                uuid: uuid.clone(),
                id: is,
            },
            KdeWindowEvent::InitialState => KdeEvent::InitialState { uuid: uuid.clone() },
            KdeWindowEvent::Unmapped => {
                // Plasma may legally send initial_state after unmapped. Keep
                // the child proxy and tombstone it until that terminal event.
                state.kde_unmapped.insert(protocol_id);
                KdeEvent::Unmapped { uuid: uuid.clone() }
            }
            _ => return,
        };
        let active_state_event = matches!(
            &event,
            KdeEvent::State { flags, .. } if flags & KDE_STATE_ACTIVE != 0
        );
        let terminal_after_unmapped = matches!(&event, KdeEvent::Unmapped { .. })
            && state
                .kde
                .as_ref()
                .is_some_and(|kde| kde.is_initialized(uuid));
        if let Some(kde) = &mut state.kde {
            if let Err(error) = kde.apply(event) {
                state.fail_kde(format!("KDE window lifecycle failed: {error}"));
                return;
            }
            if active_state_event && let Some(activation) = state.activation.take() {
                match (
                    activation.dispatched,
                    activation.tracker.verify_at(uuid, true, observed_at),
                ) {
                    (true, true) => {
                        let _ = activation.reply.send(Ok(()));
                    }
                    (true, false) if activation.tracker.timed_out(observed_at) => {
                        let _ = activation.reply.send(Err(BackendError::Unknown(
                            "activation active-state event was observed after its deadline".into(),
                        )));
                    }
                    _ => state.activation = Some(activation),
                }
            }
        }
        if terminal_after_unmapped {
            finish_kde_tombstone(state, protocol_id, uuid);
        }
        state.publish();
    }
}

fn finish_kde_tombstone(state: &mut ProtocolState, protocol_id: u32, uuid: &str) {
    state.kde_unmapped.remove(&protocol_id);
    if let Some(proxy) = state.kde_proxies.remove(&protocol_id) {
        proxy.destroy();
    } else {
        eprintln!(
            "computer-use-mcp: KDE tombstone cleanup had no proxy for protocol id {protocol_id}"
        );
    }
    state.kde_windows.remove(&protocol_id);

    let has_active_same_uuid = state
        .kde_windows
        .iter()
        .any(|(id, mapped_uuid)| mapped_uuid == uuid && !state.kde_unmapped.contains(id));
    if !has_active_same_uuid && let Some(kde) = &mut state.kde {
        kde.terminal(uuid);
    }
}

#[derive(Debug, Clone)]
pub struct WaylandCatalog {
    standard: Arc<BackendThread>,
    kde: Arc<BackendThread>,
}

#[derive(Debug, Clone)]
struct BackendThread {
    command: std::sync::mpsc::Sender<WaylandCommand>,
    wake: Option<Arc<Mutex<UnixStream>>>,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
    done: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
enum WaylandCommand {
    Snapshot(oneshot::Sender<Result<CompositorSnapshot, BackendError>>),
    Activate {
        uuid: String,
        timeout: Duration,
        reply: oneshot::Sender<Result<(), BackendError>>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
    Shutdown(oneshot::Sender<Result<(), BackendError>>),
}

impl WaylandCatalog {
    pub fn start() -> Self {
        Self {
            standard: Arc::new(BackendThread::start(ProtocolBackend::Standard)),
            kde: Arc::new(BackendThread::start(ProtocolBackend::Kde)),
        }
    }

    pub async fn snapshot(&self) -> Result<CompositorSnapshot, BackendError> {
        let standard = self.standard.snapshot().await;
        let kde = self.kde.snapshot().await;
        Ok(aggregate_snapshots(standard, kde))
    }

    pub(crate) fn begin_activation(
        &self,
        uuid: String,
        timeout: Duration,
    ) -> Result<PendingActivation, BackendError> {
        self.kde.begin_activation(uuid, timeout)
    }

    pub async fn shutdown(&self) -> Result<(), BackendError> {
        let standard = self.standard.shutdown().await;
        let kde = self.kde.shutdown().await;
        match (standard, kde) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(standard), Err(kde)) => Err(BackendError::Failed(format!(
                "standard cleanup failed: {standard}; KDE cleanup failed: {kde}"
            ))),
        }
    }
}

impl BackendThread {
    fn start(backend: ProtocolBackend) -> Self {
        let (command, receiver) = std::sync::mpsc::channel();
        let join = Arc::new(Mutex::new(None));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (wake_reader, wake_writer) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!(
                    "computer-use-mcp: Wayland {backend:?} wake channel unavailable: {error}"
                );
                drop(receiver);
                return Self {
                    command,
                    wake: None,
                    join,
                    done,
                };
            }
        };
        if let Err(error) = wake_writer.set_nonblocking(true) {
            eprintln!(
                "computer-use-mcp: Wayland {backend:?} wake channel configuration failed: {error}"
            );
            drop(receiver);
            return Self {
                command,
                wake: None,
                join,
                done,
            };
        }
        let wake = Arc::new(Mutex::new(wake_writer));
        let thread_done = Arc::clone(&done);
        let thread = match thread::Builder::new()
            .name(format!("computer-use-mcp-wayland-{backend:?}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_wayland_thread(receiver, wake_reader, backend);
                }));
                thread_done.store(true, std::sync::atomic::Ordering::Release);
                if let Err(payload) = result {
                    std::panic::resume_unwind(payload);
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                eprintln!("computer-use-mcp: Wayland {backend:?} thread unavailable: {error}");
                return Self {
                    command,
                    wake: None,
                    join,
                    done,
                };
            }
        };
        *join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thread);
        Self {
            command,
            wake: Some(wake),
            join,
            done,
        }
    }

    async fn snapshot(&self) -> Result<CompositorSnapshot, BackendError> {
        let (reply, result) = oneshot::channel();
        self.send(WaylandCommand::Snapshot(reply))?;
        match tokio::time::timeout(Duration::from_secs(3), result).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(_)) => Err(BackendError::Unavailable(
                "Wayland catalog thread stopped".into(),
            )),
            Err(_) => Err(BackendError::Unavailable(
                "Wayland catalog snapshot command exceeded its bounded deadline".into(),
            )),
        }
    }

    fn begin_activation(
        &self,
        uuid: String,
        timeout: Duration,
    ) -> Result<PendingActivation, BackendError> {
        let (result_reply, result) = oneshot::channel();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancellation = self.wake.as_ref().map(|wake| ActivationCancellation {
            cancel: Arc::clone(&cancel),
            wake: Arc::clone(wake),
        });
        let Some(cancellation) = cancellation else {
            return Err(BackendError::Unavailable(
                "Wayland catalog wake channel is unavailable".into(),
            ));
        };
        if let Err(error) = self.send_activation(WaylandCommand::Activate {
            uuid,
            timeout,
            reply: result_reply,
            cancel,
        }) {
            drop(cancellation);
            return Err(error);
        }
        Ok(PendingActivation {
            result,
            cancellation: Some(cancellation),
            timeout,
        })
    }

    async fn shutdown(&self) -> Result<(), BackendError> {
        let (reply, result) = oneshot::channel();
        let command_result = match self.send(WaylandCommand::Shutdown(reply)) {
            Err(error) => Err(error),
            Ok(()) => match tokio::time::timeout(Duration::from_secs(1), result).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(BackendError::Unavailable(
                    "Wayland shutdown reply channel closed before cleanup completed".into(),
                )),
                Err(_) => Err(BackendError::Unavailable(
                    "Wayland shutdown command exceeded its bounded deadline; cleanup status is degraded"
                        .into(),
                )),
            },
        };
        let join_result = self.join_bounded(Duration::from_secs(1)).await;
        match (command_result, join_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(command), Err(join)) => Err(BackendError::Failed(format!(
                "shutdown command failed: {command}; backend join failed: {join}"
            ))),
        }
    }

    fn send(&self, command: WaylandCommand) -> Result<(), BackendError> {
        self.send_with_wake(command, false)
    }

    fn send_activation(&self, command: WaylandCommand) -> Result<(), BackendError> {
        self.send_with_wake(command, true)
    }

    fn send_with_wake(
        &self,
        command: WaylandCommand,
        wake_must_be_confirmed: bool,
    ) -> Result<(), BackendError> {
        let Some(wake) = &self.wake else {
            return Err(BackendError::Unavailable(
                "Wayland catalog wake channel is unavailable".into(),
            ));
        };
        let mut wake = wake
            .lock()
            .map_err(|_| BackendError::Failed("Wayland catalog wake channel poisoned".into()))?;
        self.command
            .send(command)
            .map_err(|_| BackendError::Unavailable("Wayland catalog thread stopped".into()))?;
        match wake.write(&[1]) {
            Ok(1) => Ok(()),
            Ok(0) => Err(BackendError::Unknown(
                "Wayland catalog command was queued but its wake channel accepted no wake byte"
                    .into(),
            )),
            Ok(_) => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && !wake_must_be_confirmed =>
            {
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(BackendError::Unknown(
                    "Wayland activation was queued but its wake could not be confirmed".into(),
                ))
            }
            Err(error) => Err(BackendError::Unknown(format!(
                "Wayland catalog command was queued but its wake failed: {error}"
            ))),
        }
    }

    async fn join_bounded(&self, bound: Duration) -> Result<(), BackendError> {
        let deadline = Instant::now() + bound;
        while !self.thread_finished() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                eprintln!(
                    "computer-use-mcp: Wayland backend thread did not join within {bound:?}; cleanup is degraded"
                );
                return Err(BackendError::Unavailable(format!(
                    "Wayland backend thread did not join within {bound:?}; cleanup is degraded"
                )));
            }
            tokio::time::sleep(remaining.min(Duration::from_millis(5))).await;
        }
        let Some(join) = self.take_join() else {
            return Ok(());
        };
        if join.join().is_err() {
            return Err(BackendError::Failed(
                "Wayland backend thread panicked".into(),
            ));
        }
        Ok(())
    }

    fn thread_finished(&self) -> bool {
        if self.done.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        match self.join.lock() {
            Ok(join) => join
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished),
            Err(poisoned) => {
                eprintln!("computer-use-mcp: Wayland join mutex was poisoned");
                poisoned
                    .into_inner()
                    .as_ref()
                    .is_some_and(std::thread::JoinHandle::is_finished)
            }
        }
    }

    fn take_join(&self) -> Option<JoinHandle<()>> {
        match self.join.lock() {
            Ok(mut join) => join.take(),
            Err(poisoned) => {
                eprintln!("computer-use-mcp: Wayland join mutex was poisoned");
                poisoned.into_inner().take()
            }
        }
    }

    fn try_shutdown(&self) {
        let (reply, _result) = oneshot::channel();
        let _ = self.send(WaylandCommand::Shutdown(reply));
    }

    fn join_on_drop_bounded(&self, bound: Duration) {
        let deadline = std::time::Instant::now() + bound;
        while !self.thread_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let finished = self.thread_finished();
        let join = self.take_join();
        if let Some(join) = join {
            if finished || join.is_finished() {
                if join.join().is_err() {
                    eprintln!("computer-use-mcp: Wayland backend thread panicked during drop");
                }
            } else {
                eprintln!(
                    "computer-use-mcp: Wayland backend thread remains active after shutdown bound; cleanup is degraded"
                );
                drop(join);
            }
        }
    }
}

struct ActivationCancellation {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    wake: Arc<Mutex<UnixStream>>,
}

pub(crate) struct PendingActivation {
    result: oneshot::Receiver<Result<(), BackendError>>,
    cancellation: Option<ActivationCancellation>,
    timeout: Duration,
}

impl PendingActivation {
    pub(crate) async fn wait(mut self) -> Result<(), BackendError> {
        let wait = self.timeout.saturating_add(Duration::from_secs(1));
        let result = match tokio::time::timeout(wait, &mut self.result).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(BackendError::Unknown(
                "Wayland catalog thread stopped after activation dispatch".into(),
            )),
            Err(_) => Err(BackendError::Unknown(
                "Wayland activation command exceeded its bounded deadline".into(),
            )),
        };
        self.cancellation.take();
        result
    }
}

impl Drop for ActivationCancellation {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
        match self.wake.lock() {
            Ok(mut wake) => {
                let _ = wake.write(&[1]);
            }
            Err(_) => {
                eprintln!("computer-use-mcp: KDE activation cancellation wake mutex was poisoned")
            }
        }
    }
}

impl Drop for WaylandCatalog {
    fn drop(&mut self) {
        if Arc::strong_count(&self.standard) == 1 {
            self.standard.try_shutdown();
            self.standard
                .join_on_drop_bounded(Duration::from_millis(500));
        }
        if Arc::strong_count(&self.kde) == 1 {
            self.kde.try_shutdown();
            self.kde.join_on_drop_bounded(Duration::from_millis(500));
        }
    }
}

fn aggregate_snapshots(
    standard: Result<CompositorSnapshot, BackendError>,
    kde: Result<CompositorSnapshot, BackendError>,
) -> CompositorSnapshot {
    let standard = standard.unwrap_or_else(|error| CompositorSnapshot {
        records: Vec::new(),
        standard: status_from_backend_error(error),
        kde: BackendStatus::Unsupported("KDE backend uses a separate connection".into()),
    });
    let kde = kde.unwrap_or_else(|error| CompositorSnapshot {
        records: Vec::new(),
        standard: BackendStatus::Unsupported("standard backend uses a separate connection".into()),
        kde: status_from_backend_error(error),
    });
    let records = if kde.kde.is_supported() {
        kde.records
    } else {
        standard.records
    };
    CompositorSnapshot {
        records,
        standard: standard.standard,
        kde: kde.kde,
    }
}

fn status_from_backend_error(error: BackendError) -> BackendStatus {
    match error {
        BackendError::Unsupported(reason) => BackendStatus::Unsupported(reason),
        BackendError::Busy(reason) => BackendStatus::Busy(reason),
        BackendError::Unavailable(reason)
        | BackendError::Unknown(reason)
        | BackendError::Stale(reason)
        | BackendError::Failed(reason) => BackendStatus::Unavailable(reason),
    }
}

fn run_wayland_thread(
    receiver: std::sync::mpsc::Receiver<WaylandCommand>,
    mut wake: UnixStream,
    backend: ProtocolBackend,
) {
    // The protocol implementation is installed below in the generated-dispatch
    // section. Keeping initialization failure on this thread means a missing
    // compositor or single-client KDE conflict cannot affect AT-SPI capture.
    let initial = match await_initialization(backend) {
        Ok(state) => state,
        Err(error) => {
            let _ = drain_without_wayland(receiver, &mut wake, backend, error);
            return;
        }
    };
    run_protocol_loop(receiver, wake, initial);
}

fn await_initialization(backend: ProtocolBackend) -> Result<ProtocolRuntime, BackendError> {
    // Initialization stays on the event thread. This deliberately avoids a
    // detached initializer that could later create a Wayland connection after
    // the caller has already observed shutdown. The registry roundtrip itself
    // is bounded by `bounded_initial_roundtrip`; if connection setup ever
    // blocks outside that bound, the caller receives an explicit degraded
    // cleanup result from the backend join rather than a false clean shutdown.
    initialize_protocol_state(backend)
}

fn drain_without_wayland(
    receiver: std::sync::mpsc::Receiver<WaylandCommand>,
    wake: &mut UnixStream,
    backend: ProtocolBackend,
    error: BackendError,
) -> Result<(), ()> {
    let snapshot = CompositorSnapshot {
        records: Vec::new(),
        standard: match backend {
            ProtocolBackend::Standard => status_from_backend_error(error.clone()),
            ProtocolBackend::Kde => {
                BackendStatus::Unsupported("standard backend uses a separate connection".into())
            }
        },
        kde: match backend {
            ProtocolBackend::Kde => status_from_backend_error(error.clone()),
            ProtocolBackend::Standard => {
                BackendStatus::Unsupported("KDE backend uses a separate connection".into())
            }
        },
    };
    loop {
        match receiver.recv() {
            Ok(WaylandCommand::Snapshot(reply)) => {
                let _ = reply.send(Ok(snapshot.clone()));
            }
            Ok(WaylandCommand::Activate { reply, .. }) => {
                let _ = reply.send(Err(error.clone()));
            }
            Ok(WaylandCommand::Shutdown(reply)) => {
                let _ = reply.send(Ok(()));
                return Ok(());
            }
            Err(_) => return Ok(()),
        }
        let mut buffer = [0; 64];
        let _ = wake.read(&mut buffer);
    }
}

fn initialize_protocol_state(backend: ProtocolBackend) -> Result<ProtocolRuntime, BackendError> {
    let connection = Connection::connect_to_env().map_err(|error| {
        BackendError::Unavailable(format!("cannot connect to Wayland: {error}"))
    })?;
    let queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut state = ProtocolState::new(backend);
    state.registry = Some(connection.display().get_registry(&qh, ()));

    // Acquire the registry explicitly instead of using the convenience helper.
    // The helper performs an unbounded roundtrip, which would let a compositor
    // or a broken socket hold this backend thread forever. The bounded read
    // loop below also dispatches registry events into `state.globals` before
    // any protocol object is bound.
    let mut runtime = ProtocolRuntime {
        connection,
        queue,
        state,
    };
    send_initial_sync(&mut runtime);
    bounded_initial_roundtrip(&mut runtime, Duration::from_secs(2))?;

    let qh = runtime.queue.handle();
    let registry = runtime.state.registry.as_ref().ok_or_else(|| {
        BackendError::Failed("Wayland registry disappeared during initialization".into())
    })?;

    if backend == ProtocolBackend::Standard {
        match advertised_global(&runtime.state, "ext_foreign_toplevel_list_v1") {
            Some((name, version)) if version >= 1 => {
                let list =
                    registry.bind::<ExtForeignToplevelListV1, _, _>(name, version.min(1), &qh, ());
                if list.is_alive() {
                    runtime.state.foreign_list = Some(list);
                    runtime.state.snapshot.standard = BackendStatus::Supported;
                } else {
                    eprintln!(
                        "computer-use-mcp: foreign-toplevel global bind returned an inert proxy"
                    );
                    runtime.state.snapshot.standard = BackendStatus::Unavailable(
                        "foreign-toplevel global could not be bound".into(),
                    );
                }
            }
            Some((_, version)) => {
                runtime.state.snapshot.standard = BackendStatus::Unsupported(format!(
                    "KWin advertised ext-foreign-toplevel-list-v1 version {} but version 1 is required",
                    version
                ));
            }
            None => {
                runtime.state.snapshot.standard = BackendStatus::Unsupported(
                    "KWin did not provide ext-foreign-toplevel-list-v1".into(),
                );
            }
        }
    }

    if backend == ProtocolBackend::Kde && crate::window_backend::kde_rich_enabled() {
        let kde_global = advertised_global(&runtime.state, "org_kde_plasma_window_management");
        match kde_global {
            None => {
                runtime.state.snapshot.kde = BackendStatus::Unsupported(
                    "KWin did not advertise org_kde_plasma_window_management".into(),
                );
            }
            Some((_, version)) if version < KdeRichReducer::MIN_VERSION => {
                runtime.state.snapshot.kde = BackendStatus::Unsupported(format!(
                    "KDE Plasma window management version {} is below required version {}",
                    version,
                    KdeRichReducer::MIN_VERSION
                ));
            }
            Some((name, version)) => {
                let manager = registry.bind::<OrgKdePlasmaWindowManagement, _, _>(
                    name,
                    version.min(18),
                    &qh,
                    (),
                );
                if manager.is_alive() {
                    let version = manager.version();
                    runtime.state.kde = Some(KdeRichReducer::new(version).map_err(|error| {
                        BackendError::Unsupported(format!(
                            "KDE reducer rejected version {version}: {error}"
                        ))
                    })?);
                    runtime.state.kde_manager = Some(manager);
                    runtime.state.snapshot.kde = BackendStatus::Supported;
                } else {
                    eprintln!(
                        "computer-use-mcp: KDE window-management global bind returned an inert proxy"
                    );
                    runtime.state.snapshot.kde = BackendStatus::Busy(
                        "KDE rich backend could not bind its single-client global".into(),
                    );
                }
            }
        }
    } else if backend == ProtocolBackend::Kde {
        runtime.state.snapshot.kde = BackendStatus::Unsupported(format!(
            "KDE rich backend is opt-in; set {}=1",
            crate::window_backend::KDE_RICH_ENV
        ));
    }

    // The first bounded sync collected globals. A second one is required
    // after binding the manager/list so its initial child events are included
    // in the first published snapshot.
    runtime.state.initial_done = false;
    send_initial_sync(&mut runtime);
    bounded_initial_roundtrip(&mut runtime, Duration::from_secs(2))?;
    runtime.state.publish();
    Ok(runtime)
}

fn advertised_global(state: &ProtocolState, interface: &str) -> Option<(u32, u32)> {
    state
        .globals
        .iter()
        .find(|(_, global)| global.interface == interface)
        .map(|(name, global)| (*name, global.version))
}

fn send_initial_sync(runtime: &mut ProtocolRuntime) {
    let callback = runtime
        .connection
        .display()
        .sync(&runtime.queue.handle(), ());
    runtime.state.initial_sync = Some(callback);
}

fn bounded_initial_roundtrip(
    runtime: &mut ProtocolRuntime,
    bound: Duration,
) -> Result<(), BackendError> {
    use rustix::event::{PollFd, PollFlags, poll};

    let deadline = Instant::now() + bound;
    while !runtime.state.initial_done {
        if Instant::now() >= deadline {
            return Err(BackendError::Unavailable(
                "Wayland initial roundtrip exceeded its bounded deadline".into(),
            ));
        }
        runtime
            .queue
            .dispatch_pending(&mut runtime.state)
            .map_err(|error| {
                BackendError::Unavailable(format!("Wayland initial dispatch failed: {error}"))
            })?;
        if runtime.state.initial_done {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match runtime.connection.flush() {
            Ok(()) => {}
            Err(wayland_backend::client::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                let mut fds = [PollFd::new(
                    &runtime.connection,
                    PollFlags::OUT | PollFlags::ERR | PollFlags::HUP,
                )];
                let timeout = poll_timeout(remaining);
                match poll(&mut fds, Some(&timeout)) {
                    Ok(0) => {
                        return Err(BackendError::Unavailable(
                            "Wayland initial roundtrip flush exceeded its bounded deadline".into(),
                        ));
                    }
                    Ok(_) => continue,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => {
                        return Err(BackendError::Unavailable(format!(
                            "Wayland initial roundtrip writability poll failed: {error}"
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(BackendError::Unavailable(format!(
                    "Wayland initial roundtrip flush failed: {error}"
                )));
            }
        }
        let Some(guard) = runtime.queue.prepare_read() else {
            continue;
        };
        let mut fds = [PollFd::new(
            &runtime.connection,
            PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
        )];
        let timeout = poll_timeout(remaining);
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => {
                return Err(BackendError::Unavailable(
                    "Wayland initial roundtrip read exceeded its bounded deadline".into(),
                ));
            }
            Ok(_) => {
                guard.read().map_err(|error| {
                    BackendError::Unavailable(format!(
                        "Wayland initial roundtrip read failed: {error}"
                    ))
                })?;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => {
                return Err(BackendError::Unavailable(format!(
                    "Wayland initial roundtrip read poll failed: {error}"
                )));
            }
        }
    }
    Ok(())
}

const FLUSH_RETRY_BOUND: Duration = Duration::from_millis(250);

fn expire_activation(state: &mut ProtocolState, reason: impl Into<String>) {
    if let Some(activation) = state.activation.take() {
        let _ = activation
            .reply
            .send(Err(BackendError::Unknown(reason.into())));
    }
}

fn activation_cancelled(state: &ProtocolState) -> bool {
    state
        .activation
        .as_ref()
        .is_some_and(|activation| activation.cancel.load(std::sync::atomic::Ordering::Acquire))
}

fn clear_cancelled_activation(state: &mut ProtocolState) -> bool {
    if !activation_cancelled(state) {
        return false;
    }
    expire_activation(
        state,
        "KDE activation was cancelled before active-state verification",
    );
    true
}

fn activation_remaining(state: &ProtocolState) -> Option<Duration> {
    state.activation.as_ref().map(|activation| {
        activation
            .tracker
            .deadline
            .saturating_duration_since(Instant::now())
    })
}

fn poll_timeout(duration: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: duration.subsec_nanos() as _,
    }
}

fn flush_with_deadline(runtime: &mut ProtocolRuntime) -> Result<(), BackendError> {
    use rustix::event::{PollFd, PollFlags, poll};

    let fallback_deadline = Instant::now() + FLUSH_RETRY_BOUND;
    loop {
        if activation_cancelled(&runtime.state) {
            expire_activation(
                &mut runtime.state,
                "KDE activation was cancelled before request flush",
            );
            return Ok(());
        }
        match runtime.connection.flush() {
            Ok(()) => {
                if let Some(activation) = &mut runtime.state.activation {
                    activation.dispatched = true;
                }
                return Ok(());
            }
            Err(wayland_backend::client::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                let deadline = runtime
                    .state
                    .activation
                    .as_ref()
                    .map(|activation| activation.tracker.deadline)
                    .unwrap_or(fallback_deadline);
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    if runtime.state.activation.is_some() {
                        expire_activation(
                            &mut runtime.state,
                            "activation request could not be flushed before its bounded deadline",
                        );
                        return Ok(());
                    }
                    return Err(BackendError::Unavailable(
                        "Wayland request flush remained blocked past its bounded deadline".into(),
                    ));
                }
                let mut fds = [PollFd::new(
                    &runtime.connection,
                    PollFlags::OUT | PollFlags::ERR | PollFlags::HUP,
                )];
                let timeout = poll_timeout(remaining);
                match poll(&mut fds, Some(&timeout)) {
                    Ok(0) => {
                        if runtime.state.activation.is_some() {
                            expire_activation(
                                &mut runtime.state,
                                "activation request could not be flushed before its bounded deadline",
                            );
                            return Ok(());
                        }
                        return Err(BackendError::Unavailable(
                            "Wayland request flush remained blocked past its bounded deadline"
                                .into(),
                        ));
                    }
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => {}
                    Err(error) => {
                        return Err(BackendError::Failed(format!(
                            "polling Wayland writability failed while flushing: {error}"
                        )));
                    }
                }
            }
            Err(error) => {
                if runtime.state.activation.is_some() {
                    expire_activation(
                        &mut runtime.state,
                        format!("activation request flush failed after dispatch began: {error}"),
                    );
                }
                return Err(BackendError::Failed(format!(
                    "Wayland request flush failed: {error}"
                )));
            }
        }
    }
}

fn run_protocol_loop(
    receiver: std::sync::mpsc::Receiver<WaylandCommand>,
    mut wake: UnixStream,
    mut runtime: ProtocolRuntime,
) {
    use rustix::event::{PollFd, PollFlags, poll};

    let mut running = true;
    while running {
        // First drain events that were read during the previous iteration.
        // Commands are then applied, and only after that are their requests
        // flushed. This keeps all protocol state transitions on this thread.
        if let Err(error) = runtime.queue.dispatch_pending(&mut runtime.state) {
            fail_protocol_runtime(&mut runtime, format!("Wayland dispatch failed: {error}"));
            return;
        }
        if runtime
            .state
            .activation
            .as_ref()
            .is_some_and(|activation| activation.tracker.timed_out(Instant::now()))
        {
            expire_activation(
                &mut runtime.state,
                "activation request timed out before a matching active-state event",
            );
        }
        clear_cancelled_activation(&mut runtime.state);
        running = process_commands(&receiver, &mut runtime);
        if !running {
            break;
        }
        if let Err(error) = flush_with_deadline(&mut runtime) {
            fail_protocol_runtime(&mut runtime, error.to_string());
            return;
        }

        // `prepare_read` must happen before polling. Dropping the guard on a
        // wake-only or interrupted poll cancels the read preparation; consuming
        // it with `read` is reserved for a display-ready poll result.
        let Some(guard) = runtime.queue.prepare_read() else {
            continue;
        };
        let connection_fd = guard.connection_fd();
        let poll_timeout = activation_remaining(&runtime.state).map(poll_timeout);
        let poll_result = {
            let mut fds = [
                PollFd::new(
                    &connection_fd,
                    PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
                ),
                PollFd::new(&wake, PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
            ];
            match poll(&mut fds, poll_timeout.as_ref()) {
                Ok(_) => Ok((
                    fds[1]
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                    fds[0]
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                )),
                Err(error) if error == rustix::io::Errno::INTR => Err(None),
                Err(error) => Err(Some(error)),
            }
        };

        let (wake_ready, connection_ready) = match poll_result {
            Ok(ready) => ready,
            Err(None) => {
                drop(guard);
                continue;
            }
            Err(Some(error)) => {
                drop(guard);
                fail_protocol_runtime(&mut runtime, format!("Wayland poll failed: {error}"));
                return;
            }
        };

        if connection_ready {
            if let Err(error) = guard.read()
                && !matches!(
                    error,
                    wayland_backend::client::WaylandError::Io(ref io_error)
                        if io_error.kind() == std::io::ErrorKind::WouldBlock
                )
            {
                fail_protocol_runtime(&mut runtime, format!("Wayland event read failed: {error}"));
                return;
            }
            if let Err(error) = runtime.queue.dispatch_pending(&mut runtime.state) {
                fail_protocol_runtime(&mut runtime, format!("Wayland dispatch failed: {error}"));
                return;
            }
        } else {
            drop(guard);
        }

        if wake_ready {
            let mut buffer = [0; 128];
            let _ = wake.read(&mut buffer);
            running = process_commands(&receiver, &mut runtime);
        }
    }
}

fn fail_protocol_runtime(runtime: &mut ProtocolRuntime, reason: impl Into<String>) {
    runtime.state.fail_active(reason);
    if let Err(error) = flush_with_deadline(runtime) {
        eprintln!(
            "computer-use-mcp: failed to flush Wayland child-resource cleanup after backend failure: {error}"
        );
    }
}

fn process_commands(
    receiver: &std::sync::mpsc::Receiver<WaylandCommand>,
    runtime: &mut ProtocolRuntime,
) -> bool {
    let mut running = true;
    clear_cancelled_activation(&mut runtime.state);
    loop {
        let command = match receiver.try_recv() {
            Ok(command) => command,
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                eprintln!(
                    "computer-use-mcp: Wayland command channel disconnected; stopping backend thread"
                );
                runtime.state.shutdown_resources();
                if let Err(error) = flush_with_deadline(runtime) {
                    eprintln!(
                        "computer-use-mcp: Wayland cleanup flush failed after command channel disconnect: {error}"
                    );
                }
                running = false;
                break;
            }
        };
        match command {
            WaylandCommand::Snapshot(reply) => {
                runtime.state.publish();
                let _ = reply.send(Ok(runtime.state.snapshot.clone()));
            }
            WaylandCommand::Activate {
                uuid,
                timeout,
                reply,
                cancel,
            } => {
                clear_cancelled_activation(&mut runtime.state);
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    let _ = reply.send(Err(BackendError::Unknown(
                        "KDE activation was cancelled before dispatch".into(),
                    )));
                    continue;
                }
                if runtime.state.activation.is_some() {
                    let _ = reply.send(Err(BackendError::Busy(
                        "another KDE activation is awaiting verification".into(),
                    )));
                    continue;
                }
                let Some(_manager) = runtime.state.kde_manager.as_ref() else {
                    let _ = reply.send(Err(BackendError::Unsupported(
                        "KDE rich activation is not available".into(),
                    )));
                    continue;
                };
                let Some(protocol_id) = runtime
                    .state
                    .kde_windows
                    .iter()
                    .find(|(id, mapped_uuid)| {
                        mapped_uuid.as_str() == uuid.as_str()
                            && !runtime.state.kde_unmapped.contains(id)
                    })
                    .map(|(id, _)| *id)
                else {
                    let _ = reply.send(Err(BackendError::Stale(format!(
                        "KDE UUID {uuid:?} is no longer present"
                    ))));
                    continue;
                };
                let Some(window) = runtime.state.kde_proxies.get(&protocol_id) else {
                    eprintln!(
                        "computer-use-mcp: KDE catalog invariant missing proxy for protocol id {protocol_id}"
                    );
                    let _ = reply.send(Err(BackendError::Unavailable(
                        "KDE window proxy is unavailable".into(),
                    )));
                    continue;
                };
                window.set_state(KDE_STATE_ACTIVE, KDE_STATE_ACTIVE);
                runtime.state.activation = Some(ActivationWait {
                    tracker: ActivationTracker::new(uuid, Instant::now(), timeout),
                    reply,
                    dispatched: false,
                    cancel,
                });
            }
            WaylandCommand::Shutdown(reply) => {
                runtime.state.shutdown_resources();
                let cleanup = flush_with_deadline(runtime);
                if let Err(error) = &cleanup {
                    eprintln!(
                        "computer-use-mcp: Wayland resource cleanup flush failed during shutdown: {error}"
                    );
                }
                let _ = reply.send(cleanup);
                running = false;
                break;
            }
        }
    }
    running
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_backend::CapabilityState;

    #[test]
    fn foreign_lifecycle_requires_done_and_identifier_and_removes_closed() {
        let mut reducer = ForeignToplevelReducer::default();
        reducer
            .apply(ForeignEvent::Created {
                handle: "h1".into(),
            })
            .unwrap();
        reducer
            .apply(ForeignEvent::Title {
                handle: "h1".into(),
                title: "Editor".into(),
            })
            .unwrap();
        assert!(reducer.snapshot().unwrap().is_empty());
        reducer
            .apply(ForeignEvent::Identifier {
                handle: "h1".into(),
                identifier: "uuid-1".into(),
            })
            .unwrap();
        assert!(reducer.snapshot().unwrap().is_empty());
        reducer
            .apply(ForeignEvent::Done {
                handle: "h1".into(),
            })
            .unwrap();
        let records = reducer.snapshot().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].backend_identity, "uuid-1");
        assert!(records[0].pid.is_none());
        assert!(matches!(
            records[0].capabilities.activate,
            CapabilityState::Unsupported { .. }
        ));
        reducer
            .apply(ForeignEvent::Closed {
                handle: "h1".into(),
            })
            .unwrap();
        assert!(reducer.snapshot().unwrap().is_empty());
    }

    #[test]
    fn foreign_identifier_accepts_printable_ascii_up_to_protocol_limit() {
        assert!(!valid_foreign_identifier(""));
        assert!(valid_foreign_identifier("a"));
        assert!(valid_foreign_identifier(&"x".repeat(32)));
        assert!(!valid_foreign_identifier(&"x".repeat(33)));
        assert!(!valid_foreign_identifier("line\nfeed"));
        assert!(!valid_foreign_identifier("café"));

        let mut reducer = ForeignToplevelReducer::default();
        reducer
            .apply(ForeignEvent::Created {
                handle: "valid".into(),
            })
            .unwrap();
        reducer
            .apply(ForeignEvent::Identifier {
                handle: "valid".into(),
                identifier: "good".into(),
            })
            .unwrap();
        reducer
            .apply(ForeignEvent::Created {
                handle: "invalid".into(),
            })
            .unwrap();
        assert!(matches!(
            reducer.apply(ForeignEvent::Identifier {
                handle: "invalid".into(),
                identifier: "bad\nvalue".into(),
            }),
            Err(ReducerError::InvalidIdentity(identity)) if identity == "bad\nvalue"
        ));
        reducer
            .apply(ForeignEvent::Done {
                handle: "valid".into(),
            })
            .unwrap();
        assert_eq!(reducer.snapshot().unwrap().len(), 1);
    }

    #[test]
    fn foreign_duplicate_protocol_identifier_fails_closed_without_joining() {
        let mut reducer = ForeignToplevelReducer::default();
        reducer
            .apply(ForeignEvent::Created {
                handle: "h1".into(),
            })
            .unwrap();
        assert!(matches!(
            reducer.apply(ForeignEvent::Created {
                handle: "h1".into(),
            }),
            Err(ReducerError::DuplicateIdentity(identity)) if identity == "h1"
        ));
        reducer
            .apply(ForeignEvent::Closed {
                handle: "h1".into(),
            })
            .unwrap();

        for handle in ["h1", "h2"] {
            reducer
                .apply(ForeignEvent::Created {
                    handle: handle.into(),
                })
                .unwrap();
            reducer
                .apply(ForeignEvent::Identifier {
                    handle: handle.into(),
                    identifier: "same".into(),
                })
                .unwrap();
            reducer
                .apply(ForeignEvent::Done {
                    handle: handle.into(),
                })
                .unwrap();
        }
        assert!(matches!(
            reducer.snapshot(),
            Err(ReducerError::DuplicateIdentity(identity)) if identity == "same"
        ));
    }

    #[test]
    fn foreign_updates_publish_atomically_and_finished_clears_records() {
        let mut reducer = ForeignToplevelReducer::default();
        reducer
            .apply(ForeignEvent::Created {
                handle: "h1".into(),
            })
            .unwrap();
        reducer
            .apply(ForeignEvent::Identifier {
                handle: "h1".into(),
                identifier: "window-1".into(),
            })
            .unwrap();
        reducer
            .apply(ForeignEvent::Title {
                handle: "h1".into(),
                title: "first".into(),
            })
            .unwrap();
        reducer
            .apply(ForeignEvent::Done {
                handle: "h1".into(),
            })
            .unwrap();
        assert_eq!(reducer.snapshot().unwrap()[0].title, "first");
        reducer
            .apply(ForeignEvent::Title {
                handle: "h1".into(),
                title: "second".into(),
            })
            .unwrap();
        assert_eq!(reducer.snapshot().unwrap()[0].title, "first");
        reducer
            .apply(ForeignEvent::Done {
                handle: "h1".into(),
            })
            .unwrap();
        assert_eq!(reducer.snapshot().unwrap()[0].title, "second");
        reducer.apply(ForeignEvent::Finished).unwrap();
        assert!(reducer.snapshot().unwrap().is_empty());
        assert!(reducer.is_finished());
    }

    #[test]
    fn kde_reducer_tracks_protocol_versioned_fields_and_uuid_lifecycle() {
        let mut reducer = KdeRichReducer::new(17).unwrap();
        reducer
            .apply(KdeEvent::Created { uuid: "u1".into() })
            .unwrap();
        reducer
            .apply(KdeEvent::AppId {
                uuid: "u1".into(),
                app_id: "org.example.Editor".into(),
            })
            .unwrap();
        reducer
            .apply(KdeEvent::Pid {
                uuid: "u1".into(),
                pid: 42,
            })
            .unwrap();
        reducer
            .apply(KdeEvent::Geometry {
                uuid: "u1".into(),
                x: 10,
                y: 20,
                width: 800,
                height: 600,
                client: false,
            })
            .unwrap();
        reducer
            .apply(KdeEvent::VirtualDesktopEntered {
                uuid: "u1".into(),
                id: "desktop-a".into(),
            })
            .unwrap();
        reducer
            .apply(KdeEvent::State {
                uuid: "u1".into(),
                flags: KDE_STATE_ACTIVE | KDE_STATE_RESIZABLE,
            })
            .unwrap();
        assert!(reducer.snapshot().is_empty());
        reducer
            .apply(KdeEvent::InitialState { uuid: "u1".into() })
            .unwrap();
        let records = reducer.snapshot();
        assert_eq!(records[0].backend_identity, "u1");
        assert_eq!(records[0].pid, Some(42));
        assert!(records[0].states.contains("active"));
        assert_eq!(records[0].virtual_desktops, ["desktop-a"]);
        assert!(records[0].capabilities.screenshot.is_supported());
        assert!(reducer.is_active("u1").unwrap());
        reducer
            .apply(KdeEvent::Unmapped { uuid: "u1".into() })
            .unwrap();
        assert!(reducer.snapshot().is_empty());
    }

    #[test]
    fn kde_reducer_preserves_unknown_flags_and_accepts_early_unmapped() {
        assert!(KdeRichReducer::new(16).is_err());
        let mut reducer = KdeRichReducer::new(17).unwrap();
        reducer
            .apply(KdeEvent::Created { uuid: "u1".into() })
            .unwrap();
        reducer
            .apply(KdeEvent::Unmapped { uuid: "u1".into() })
            .unwrap();
        reducer
            .apply(KdeEvent::State {
                uuid: "u1".into(),
                flags: 1 << 31,
            })
            .unwrap();
        reducer
            .apply(KdeEvent::InitialState { uuid: "u1".into() })
            .unwrap();
        assert!(reducer.snapshot().is_empty());

        reducer.remap("u1".into()).unwrap();
        assert!(!reducer.is_unmapped("u1"));
        reducer
            .apply(KdeEvent::InitialState { uuid: "u1".into() })
            .unwrap();
        assert_eq!(reducer.snapshot().len(), 1);
    }

    #[test]
    fn activation_tracker_requires_exact_uuid_and_subsequent_active_event() {
        let now = Instant::now();
        let tracker = ActivationTracker::new("uuid-1", now, Duration::from_millis(10));
        assert!(!tracker.verify("uuid-2", true));
        assert!(!tracker.verify("uuid-1", false));
        assert!(tracker.verify("uuid-1", true));
        assert!(tracker.verify_at("uuid-1", true, now + Duration::from_millis(9)));
        assert!(!tracker.verify_at("uuid-1", true, now + Duration::from_millis(10)));
        assert!(!tracker.verify_at("uuid-1", true, now + Duration::from_millis(11)));
        assert!(!tracker.timed_out(now));
        assert!(tracker.timed_out(now + Duration::from_millis(10)));
    }

    #[test]
    fn activation_deadline_is_unknown_after_dispatch_and_backend_failures_are_isolated() {
        let (reply, mut result) = oneshot::channel();
        let mut kde = ProtocolState::new(ProtocolBackend::Kde);
        kde.activation = Some(ActivationWait {
            tracker: ActivationTracker::new("u1", Instant::now(), Duration::ZERO),
            reply,
            dispatched: true,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        expire_activation(&mut kde, "bounded activation deadline elapsed");
        assert!(matches!(
            result.try_recv(),
            Ok(Err(BackendError::Unknown(reason))) if reason.contains("bounded")
        ));

        let mut standard = ProtocolState::new(ProtocolBackend::Standard);
        standard.snapshot.standard = BackendStatus::Supported;
        standard.fail_active("standard protocol failed");
        assert!(matches!(
            standard.snapshot.standard,
            BackendStatus::Unavailable(reason) if reason.contains("standard")
        ));
        assert!(matches!(
            standard.snapshot.kde,
            BackendStatus::Unsupported(_)
        ));
        assert!(poll_timeout(Duration::from_millis(5)).tv_nsec > 0);
    }

    #[test]
    fn cancelled_activation_is_cleared_before_a_subsequent_activation() {
        let (first_reply, _first_result) = oneshot::channel();
        let first_cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut kde = ProtocolState::new(ProtocolBackend::Kde);
        kde.activation = Some(ActivationWait {
            tracker: ActivationTracker::new("u1", Instant::now(), Duration::from_secs(1)),
            reply: first_reply,
            dispatched: true,
            cancel: first_cancel,
        });

        assert!(clear_cancelled_activation(&mut kde));
        assert!(kde.activation.is_none());

        let (second_reply, _second_result) = oneshot::channel();
        kde.activation = Some(ActivationWait {
            tracker: ActivationTracker::new("u2", Instant::now(), Duration::from_secs(1)),
            reply: second_reply,
            dispatched: false,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        assert!(!clear_cancelled_activation(&mut kde));
        assert_eq!(kde.activation.as_ref().unwrap().tracker.uuid, "u2");
    }

    #[tokio::test]
    async fn bounded_join_reports_degraded_cleanup_instead_of_clean_success() {
        let (command, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        let join = Arc::new(Mutex::new(Some(thread::spawn(|| {
            thread::sleep(Duration::from_millis(100));
        }))));
        let backend = BackendThread {
            command,
            wake: None,
            join,
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let error = backend
            .join_bounded(Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("degraded"));
    }

    #[tokio::test]
    async fn backend_thread_panic_becomes_a_stable_error() {
        let (command, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        let join = Arc::new(Mutex::new(Some(thread::spawn(|| {
            panic!("protocol lifecycle panic");
        }))));
        let backend = BackendThread {
            command,
            wake: None,
            join,
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let error = backend
            .join_bounded(Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn queued_activation_with_failed_wake_is_unknown() {
        let (command, receiver) = std::sync::mpsc::channel();
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(reader);
        let backend = BackendThread {
            command,
            wake: Some(Arc::new(Mutex::new(writer))),
            join: Arc::new(Mutex::new(None)),
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (reply, _result) = oneshot::channel();
        let error = backend
            .send(WaylandCommand::Activate {
                uuid: "u1".into(),
                timeout: Duration::from_secs(1),
                reply,
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
            .unwrap_err();
        assert!(matches!(
            error,
            BackendError::Unknown(reason) if reason.contains("command was queued")
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(WaylandCommand::Activate { uuid, .. }) if uuid == "u1"
        ));
    }

    #[test]
    fn closed_wayland_socket_reports_flush_failure_for_shutdown_cleanup() {
        let (client, peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue: EventQueue<ProtocolState> = connection.new_event_queue();
        let callback = connection.display().sync(&queue.handle(), ());
        drop(callback);
        drop(peer);
        let error = connection.flush().unwrap_err();
        assert!(error.to_string().contains("I/O") || error.to_string().contains("Broken pipe"));
    }
}
