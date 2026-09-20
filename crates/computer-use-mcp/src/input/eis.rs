use std::{
    collections::HashMap,
    io::{Read, Seek},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use reis::{
    Interface, PendingRequestResult, ei,
    event::{Device, DeviceCapability, EiEvent, EiEventConverter},
};
use rustix::time::{ClockId, clock_gettime};
use tokio::sync::{Notify, mpsc, oneshot};
use xkbcommon::xkb;

use crate::portal::PortalSessionLease;

use super::{
    InputMode,
    backend::{HeldInput, InputBackend, InputEvent, InputFuture, KeyboardKey},
    coordinates::{
        EisRegion, EisRoute, StreamExtent, describe_region, resolve_union_tiling_with_extent,
    },
};

const READY_TIMEOUT: Duration = Duration::from_secs(3);
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedKey {
    pub device_id: u64,
    pub resume_generation: u64,
    pub keycode: u32,
    pub modifiers: [Option<u32>; 4],
}

struct DeviceState {
    device: Device,
    keymap: Option<String>,
    resumed: bool,
    modifiers_synced: bool,
    resume_generation: u64,
    sequence: u32,
    modifiers: Option<(u32, u32, u32, u32)>,
    emulating: bool,
}

impl DeviceState {
    fn is_usable_keyboard(&self) -> bool {
        self.keymap.is_some() && self.device.interface::<ei::Keyboard>().is_some()
    }

    /// Ready to type: resumed, modifier-synchronized with a known state,
    /// and usable. This is the shared keyboard predicate for the
    /// pointer-seat and focused-element paths.
    fn is_ready_keyboard(&self) -> bool {
        self.resumed
            && self.modifiers_synced
            && self.modifiers.is_some()
            && self.is_usable_keyboard()
    }
}

struct EisState {
    connection: Option<reis::event::Connection>,
    devices: HashMap<u64, DeviceState>,
    terminal: Option<String>,
    binding: Option<EisBinding>,
    /// Keyboard bound by [`ReisInputBackend::wait_for_keyboard`] for
    /// focused-element typing. Unlike [`EisBinding`] it is anchored on the
    /// unique usable keyboard instead of the pointer seat, so typing works
    /// even when no single pointer region matches (e.g. ambiguous multi-
    /// monitor EIS advertisements). Exactly-one-keyboard keeps it fail-closed.
    focused_keyboard: Option<FocusedKeyboardBinding>,
    mode: Option<InputMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FocusedKeyboardBinding {
    device_id: u64,
    resume_generation: u64,
}

impl EisState {
    fn new() -> Self {
        Self {
            connection: None,
            devices: HashMap::new(),
            terminal: None,
            binding: None,
            focused_keyboard: None,
            mode: None,
        }
    }

    fn bind_pointer_mode(&mut self, binding: EisBinding) {
        self.binding = Some(binding);
        self.focused_keyboard = None;
        self.mode = Some(InputMode::Pointer);
    }

    fn bind_focused_keyboard_mode(&mut self, binding: FocusedKeyboardBinding) {
        self.focused_keyboard = Some(binding);
        self.binding = None;
        self.mode = Some(InputMode::FocusedKeyboard);
    }

    /// Exactly-one device selection shared by the keyboard and scroll
    /// lookups. `Ok(None)` means nothing usable is resumed yet (callers keep
    /// waiting); more than one match fails closed as ambiguous input.
    fn unique_device(
        &self,
        ambiguous_noun: &str,
        mut matches: impl FnMut(&DeviceState) -> bool,
    ) -> Result<Option<u64>, String> {
        let mut matched = Vec::new();
        for (&id, state) in &self.devices {
            if matches(state) {
                matched.push(id);
            }
        }
        match matched.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some(*id)),
            many => Err(format!(
                "{} {ambiguous_noun}; refusing ambiguous input",
                many.len()
            )),
        }
    }

    /// The unique resumed, synchronized, usable keyboard across all EIS
    /// devices, without any pointer-seat anchoring. `Ok(None)` means no
    /// keyboard is ready yet (keep waiting); `Err` means two or more match
    /// and typing would be ambiguous (fail closed immediately).
    fn unique_keyboard(&self) -> Result<Option<u64>, String> {
        self.unique_device(
            "resumed and synchronized EIS keyboards are available",
            DeviceState::is_ready_keyboard,
        )
    }

    /// Validate the keyboard bound for focused-element typing: it must still
    /// exist with the same resume generation, still be resumed/synchronized/
    /// usable, and still be the unique keyboard. Any drift fails closed.
    fn focused_keyboard_device(&self) -> Result<u64, String> {
        let binding = self.focused_keyboard.as_ref().ok_or_else(|| {
            "focused-element input was not prepared: no EIS keyboard is bound".to_owned()
        })?;
        let device = self
            .devices
            .get(&binding.device_id)
            .ok_or_else(|| "bound EIS keyboard disappeared".to_owned())?;
        if device.resume_generation != binding.resume_generation {
            return Err("bound EIS keyboard changed during focused input".into());
        }
        if !device.is_ready_keyboard() {
            return Err("bound EIS keyboard is no longer resumed and synchronized".into());
        }
        match self.unique_keyboard()? {
            Some(id) if id == binding.device_id => Ok(id),
            _ => Err("available EIS keyboards changed during focused input".into()),
        }
    }

    /// Every resumed absolute-pointer region matching the stream route.
    /// Empty means no usable device is resumed yet (callers keep waiting);
    /// more than one means the compositor advertised an ambiguous
    /// multi-monitor layout (callers refuse, or resolve via
    /// [`ReisInputBackend::resolve_pointer_binding`] with action geometry).
    fn matched_pointer_regions(&self, route: &EisRoute) -> Vec<MatchedPointerRegion> {
        let mut matched = Vec::new();
        for (&id, state) in &self.devices {
            if !state.resumed || state.device.interface::<ei::PointerAbsolute>().is_none() {
                continue;
            }
            for region in state.device.regions() {
                let region = EisRegion {
                    position: (region.x, region.y),
                    size: (region.width, region.height),
                    mapping_id: region.mapping_id.clone(),
                };
                if route.matches(&region) {
                    matched.push(MatchedPointerRegion {
                        device_id: id,
                        resume_generation: state.resume_generation,
                        region,
                    });
                }
            }
        }
        matched
    }

    fn scroll_device(&self, pointer_device: u64) -> Result<Option<u64>, String> {
        let pointer = self
            .devices
            .get(&pointer_device)
            .ok_or("selected EIS pointer disappeared")?;
        if pointer.device.interface::<ei::Scroll>().is_some() {
            return Ok(Some(pointer_device));
        }
        let pointer_seat = pointer.device.seat();
        self.unique_device(
            "resumed EIS scroll devices share the selected pointer seat",
            |state| {
                state.resumed
                    && state.device.seat() == pointer_seat
                    && state.device.interface::<ei::Scroll>().is_some()
            },
        )
    }

    fn keyboard_device(&self, pointer_device: u64) -> Result<Option<u64>, String> {
        let pointer_seat = self
            .devices
            .get(&pointer_device)
            .ok_or("selected EIS pointer disappeared")?
            .device
            .seat();
        self.unique_device(
            "resumed and synchronized EIS keyboards are available",
            |state| state.is_ready_keyboard() && state.device.seat() == pointer_seat,
        )
    }

    fn keyboard_diagnostics(&self, route: &EisRoute) -> Result<String, String> {
        let pointer_id = self
            .matched_pointer_regions(route)
            .first()
            .ok_or("exact EIS pointer region is not resumed")?
            .device_id;
        let seat = self
            .devices
            .get(&pointer_id)
            .ok_or("selected EIS pointer disappeared")?
            .device
            .seat();
        Ok(describe_keyboards(Some(seat), &self.devices))
    }
}

/// One-line-per-keyboard status shared by the pointer-anchored and
/// focused-element timeout errors. With `seat`, each line gains a
/// same-seat column for the pointer-anchored path.
fn describe_keyboards(
    seat: Option<&reis::event::Seat>,
    devices: &HashMap<u64, DeviceState>,
) -> String {
    let mut details = Vec::new();
    for (id, state) in devices {
        if state.device.interface::<ei::Keyboard>().is_none() {
            continue;
        }
        let mut line = format!(
            "device {id}: resumed={} keymap={} modifiers={} synchronized={}",
            state.resumed,
            state.keymap.is_some(),
            state.modifiers.is_some(),
            state.modifiers_synced
        );
        if let Some(seat) = seat {
            line.push_str(&format!(" same_seat={}", state.device.seat() == seat));
        }
        details.push(line);
    }
    if details.is_empty() {
        "no EIS keyboard device was advertised".to_owned()
    } else {
        details.join("; ")
    }
}

struct EisAttemptGuard {
    session: Arc<PortalSessionLease>,
    armed: bool,
}

struct SyncRequest {
    keyboard_id: Option<u64>,
    response: oneshot::Sender<Result<(), String>>,
}

#[derive(Default)]
struct CleanupState {
    held: Vec<HeldInput>,
    sequence_pending: bool,
    mode: Option<InputMode>,
}

#[derive(Clone, PartialEq, Eq)]
struct EisBinding {
    pointer_id: u64,
    resume_generation: u64,
    region: EisRegion,
    /// Source regions backing a union binding, sorted for deterministic
    /// comparison. Empty for the legacy single-region path.
    union_sources: Vec<EisRegion>,
}

struct MatchedPointerRegion {
    device_id: u64,
    resume_generation: u64,
    region: EisRegion,
}

/// Region bound for one pointer action: the EIS region absolute motion is
/// mapped into, plus optional union-disambiguation evidence for the action
/// output when several resumed regions were resolved.
#[derive(Debug, Clone)]
pub struct ResolvedPointerBinding {
    pub region: EisRegion,
    pub evidence: Option<String>,
}

fn sort_regions(regions: &mut [EisRegion]) {
    regions.sort_by(|first, second| {
        (first.position, first.size, first.mapping_id.clone()).cmp(&(
            second.position,
            second.size,
            second.mapping_id.clone(),
        ))
    });
}

/// List every matching region so the ambiguity can be diagnosed from the
/// error alone (multi-monitor KWin setups may advertise duplicate regions).
fn ambiguous_regions_error(route: &EisRoute, matched: &[MatchedPointerRegion]) -> String {
    let details = matched
        .iter()
        .map(|candidate| {
            format!(
                "device {} gen={} region=({},{}) {}x{} mapping_id={:?}",
                candidate.device_id,
                candidate.resume_generation,
                candidate.region.position.0,
                candidate.region.position.1,
                candidate.region.size.0,
                candidate.region.size.1,
                candidate.region.mapping_id,
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "multiple resumed EIS regions match the selected monitor stream (route={route:?}); refusing ambiguous input: {details}"
    )
}

struct EisThread {
    shutdown: Option<UnixStream>,
    handle: Option<std::thread::JoinHandle<()>>,
    done: std::sync::mpsc::Receiver<()>,
    stopping: Arc<AtomicBool>,
}

impl EisAttemptGuard {
    fn new(session: Arc<PortalSessionLease>) -> Result<Self, String> {
        session.begin_eis_attempt()?;
        Ok(Self {
            session,
            armed: true,
        })
    }

    fn complete(mut self) -> Result<(), String> {
        self.session.complete_eis_attempt()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for EisAttemptGuard {
    fn drop(&mut self) {
        if self.armed {
            self.session
                .invalidate_eis("ConnectToEIS setup was interrupted or failed");
        }
    }
}

pub struct ReisInputBackend {
    session: Arc<PortalSessionLease>,
    route: EisRoute,
    state: Arc<Mutex<EisState>>,
    ready: Arc<Notify>,
    cleanup: Mutex<CleanupState>,
    serial: tokio::sync::Mutex<()>,
    thread: Mutex<EisThread>,
    sync_requests: mpsc::UnboundedSender<SyncRequest>,
}

impl ReisInputBackend {
    pub async fn connect(
        session: Arc<PortalSessionLease>,
        stream: &crate::portal::PortalStream,
    ) -> Result<Arc<Self>, String> {
        let route = EisRoute::from_stream(stream)?;
        if let EisRoute::ExactGeometry { position, size } = &route {
            eprintln!(
                "computer-use-mcp: ScreenCast stream has no mapping_id; binding EIS input by exact geometry at ({}, {}) with size {}x{}",
                position.0, position.1, size.0, size.1
            );
        } else if matches!(&route, EisRoute::UniqueResumedRegion) {
            eprintln!(
                "computer-use-mcp: ScreenCast stream has no routing metadata; binding EIS input to the unique resumed monitor region"
            );
        }
        let attempt = EisAttemptGuard::new(Arc::clone(&session))?;
        let socket = session.connect_to_eis().await?;
        let shutdown = socket
            .try_clone()
            .map_err(|error| format!("cannot clone EIS shutdown socket: {error}"))?;
        let state = Arc::new(Mutex::new(EisState::new()));
        let ready = Arc::new(Notify::new());
        let thread_state = Arc::clone(&state);
        let thread_ready = Arc::clone(&ready);
        let thread_session = Arc::clone(&session);
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = Arc::clone(&stopping);
        let (sync_requests, sync_receiver) = mpsc::unbounded_channel();
        let (done_sender, thread_done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("computer-use-mcp-eis".into())
            .spawn(move || {
                run_eis(
                    socket,
                    thread_state,
                    thread_ready,
                    thread_session,
                    thread_stopping,
                    sync_receiver,
                );
                let _ = done_sender.send(());
            })
            .map_err(|error| format!("cannot start EIS event thread: {error}"))?;
        let backend = Arc::new(Self {
            session,
            route,
            state,
            ready,
            cleanup: Mutex::new(CleanupState::default()),
            serial: tokio::sync::Mutex::new(()),
            thread: Mutex::new(EisThread {
                shutdown: Some(shutdown),
                handle: Some(thread),
                done: thread_done,
                stopping,
            }),
            sync_requests,
        });
        attempt.complete()?;
        Ok(backend)
    }

    /// Bind one pointer region unambiguously; several resumed regions from
    /// the same device are resolved through the action's PNG geometry (see
    /// [`resolve_union_tiling_with_extent`]). `None` means nothing usable is
    /// resumed yet (callers keep waiting).
    fn resolve_pointer_binding(
        route: &EisRoute,
        matched: &[MatchedPointerRegion],
        output_size: (u32, u32),
        stream_extent: Option<StreamExtent>,
        window_cropped: bool,
        png_points: &[(f64, f64)],
    ) -> Result<Option<(EisBinding, Option<String>)>, String> {
        let [only] = matched else {
            if matched.is_empty() {
                return Ok(None);
            }
            let first = &matched[0];
            if matched.iter().any(|candidate| {
                candidate.device_id != first.device_id
                    || candidate.resume_generation != first.resume_generation
            }) {
                return Err(ambiguous_regions_error(route, matched));
            }
            let regions: Vec<EisRegion> = matched
                .iter()
                .map(|candidate| candidate.region.clone())
                .collect();
            let stream_extent = stream_extent.ok_or_else(|| {
                "ambiguous EIS regions have no authoritative portal stream extent; refusing union resolution"
                    .to_owned()
            })?;
            let (union, containing) = resolve_union_tiling_with_extent(
                &regions,
                output_size,
                stream_extent,
                window_cropped,
                png_points,
            )?;
            let mut union_sources = regions;
            sort_regions(&mut union_sources);
            return Ok(Some((
                EisBinding {
                    pointer_id: first.device_id,
                    resume_generation: first.resume_generation,
                    region: union,
                    union_sources,
                },
                Some(describe_region(&containing)),
            )));
        };
        Ok(Some((
            EisBinding {
                pointer_id: only.device_id,
                resume_generation: only.resume_generation,
                region: only.region.clone(),
                union_sources: Vec::new(),
            },
            None,
        )))
    }

    async fn wait_resolved_ready(
        &self,
        keyboard_required: bool,
        output_size: (u32, u32),
        stream_extent: Option<StreamExtent>,
        window_cropped: bool,
        png_points: &[(f64, f64)],
    ) -> Result<ResolvedPointerBinding, String> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "EIS state mutex poisoned".to_owned())?;
                if let Some(error) = &state.terminal {
                    return Err(error.clone());
                }
                let matched = state.matched_pointer_regions(&self.route);
                if let Some((binding, evidence)) = Self::resolve_pointer_binding(
                    &self.route,
                    &matched,
                    output_size,
                    stream_extent,
                    window_cropped,
                    png_points,
                )? {
                    let keyboard_ready =
                        !keyboard_required || state.keyboard_device(binding.pointer_id)?.is_some();
                    if keyboard_ready {
                        if state
                            .binding
                            .as_ref()
                            .is_some_and(|selected| selected != &binding)
                        {
                            return Err(
                                "selected EIS region changed during the portal session".into()
                            );
                        }
                        let region = binding.region.clone();
                        state.bind_pointer_mode(binding);
                        return Ok(ResolvedPointerBinding { region, evidence });
                    }
                }
            }
            self.ready.notified().await;
        }
    }

    pub async fn wait_for_resolved_action(
        &self,
        keyboard_required: bool,
        output_size: (u32, u32),
        stream_extent: Option<StreamExtent>,
        window_cropped: bool,
        png_points: &[(f64, f64)],
    ) -> Result<ResolvedPointerBinding, String> {
        let _serial = self.serial.lock().await;
        self.ensure_mode_switch_allowed()?;
        tokio::time::timeout(
            READY_TIMEOUT,
            self.wait_resolved_ready(
                keyboard_required,
                output_size,
                stream_extent,
                window_cropped,
                png_points,
            ),
        )
        .await
        .map_err(|_| {
            if keyboard_required {
                let detail = self
                    .state
                    .lock()
                    .map_err(|_| "EIS state mutex poisoned".to_owned())
                    .and_then(|state| state.keyboard_diagnostics(&self.route))
                    .unwrap_or_else(|error| error);
                format!(
                    "timed out waiting for a synchronized EIS keyboard on the monitor seat ({detail})"
                )
            } else {
                "timed out waiting for the exact EIS monitor device".to_owned()
            }
        })?
    }

    pub fn resolved_region(
        &self,
        output_size: (u32, u32),
        stream_extent: Option<StreamExtent>,
        window_cropped: bool,
        png_points: &[(f64, f64)],
    ) -> Result<ResolvedPointerBinding, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        if let Some(error) = &state.terminal {
            return Err(error.clone());
        }
        let bound = self.pointer_device_for_action(&state)?;
        // Re-resolve deterministically and require the same binding, so a
        // region-set or geometry change between preparation and dispatch
        // refuses instead of misdelivering.
        let matched = state.matched_pointer_regions(&self.route);
        let recomputed = Self::resolve_pointer_binding(
            &self.route,
            &matched,
            output_size,
            stream_extent,
            window_cropped,
            png_points,
        )?
        .ok_or_else(|| "exact EIS pointer region is no longer resumed".to_owned())?;
        if recomputed.0 != bound {
            return Err("selected EIS region changed before input dispatch".into());
        }
        Ok(ResolvedPointerBinding {
            region: bound.region,
            evidence: recomputed.1,
        })
    }

    /// Wait for the unique usable EIS keyboard and bind it for
    /// focused-element typing. No pointer region is resolved, so ambiguous
    /// multi-monitor pointer advertisements do not block typing into an
    /// AT-SPI-verified focused element.
    pub async fn wait_for_keyboard(&self) -> Result<u64, String> {
        let _serial = self.serial.lock().await;
        self.ensure_mode_switch_allowed()?;
        tokio::time::timeout(READY_TIMEOUT, self.wait_keyboard_ready())
            .await
            .map_err(|_| self.keyboard_wait_detail())?
    }

    async fn wait_keyboard_ready(&self) -> Result<u64, String> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "EIS state mutex poisoned".to_owned())?;
                if let Some(error) = &state.terminal {
                    return Err(error.clone());
                }
                if let Some(id) = state.unique_keyboard()? {
                    let resume_generation = state
                        .devices
                        .get(&id)
                        .ok_or_else(|| "selected EIS keyboard disappeared".to_owned())?
                        .resume_generation;
                    state.bind_focused_keyboard_mode(FocusedKeyboardBinding {
                        device_id: id,
                        resume_generation,
                    });
                    return Ok(id);
                }
            }
            self.ready.notified().await;
        }
    }

    fn keyboard_wait_detail(&self) -> String {
        let detail = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())
            .map(|state| describe_keyboards(None, &state.devices))
            .unwrap_or_else(|error| error);
        format!("timed out waiting for a unique synchronized EIS keyboard ({detail})")
    }

    fn ensure_mode_switch_allowed(&self) -> Result<(), String> {
        let cleanup = self
            .cleanup
            .lock()
            .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?;
        if cleanup.sequence_pending || !cleanup.held.is_empty() {
            return Err("cannot switch EIS input modes during an active transaction".into());
        }
        Ok(())
    }

    /// Capability gate for focused-element typing: the bound keyboard must
    /// still be present, unchanged, resumed, and unique.
    pub fn require_focused_keyboard(&self) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        if state.mode != Some(InputMode::FocusedKeyboard) {
            return Err("focused-element input mode was not prepared".into());
        }
        state.focused_keyboard_device().map(drop)
    }

    fn pointer_device_for_action(&self, state: &EisState) -> Result<EisBinding, String> {
        if state.mode != Some(InputMode::Pointer) {
            return Err("pointer input mode was not prepared".into());
        }
        // Structural validation only (no PNG geometry here): the bound
        // single region must still be the unique match, or the bound union
        // must still equal the union of the same-device match set.
        let bound = state
            .binding
            .as_ref()
            .ok_or("EIS monitor region was not bound during input preparation")?
            .clone();
        let matched = state.matched_pointer_regions(&self.route);
        if matched.is_empty() {
            return Err("exact EIS pointer region is no longer resumed".into());
        }
        if bound.union_sources.is_empty() {
            if matched.len() == 1
                && matched[0].device_id == bound.pointer_id
                && matched[0].resume_generation == bound.resume_generation
                && matched[0].region == bound.region
            {
                return Ok(bound);
            }
            return Err("selected EIS region changed before input dispatch".into());
        }
        if matched.iter().any(|candidate| {
            candidate.device_id != bound.pointer_id
                || candidate.resume_generation != bound.resume_generation
        }) {
            return Err("selected EIS region changed before input dispatch".into());
        }
        let mut regions: Vec<EisRegion> = matched
            .iter()
            .map(|candidate| candidate.region.clone())
            .collect();
        sort_regions(&mut regions);
        if regions != bound.union_sources {
            return Err("selected EIS region changed before input dispatch".into());
        }
        Ok(bound)
    }

    pub fn require_capabilities(
        &self,
        button: bool,
        scroll: bool,
        keyboard: bool,
    ) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        match state.mode {
            Some(InputMode::Pointer) => {
                let pointer = self.pointer_device_for_action(&state)?.pointer_id;
                let pointer_state = state
                    .devices
                    .get(&pointer)
                    .ok_or("selected EIS pointer disappeared")?;
                if button && pointer_state.device.interface::<ei::Button>().is_none()
                    || scroll && state.scroll_device(pointer)?.is_none()
                    || keyboard && state.keyboard_device(pointer)?.is_none()
                {
                    return Err(
                        "EIS backend lacks the device capabilities required for this action".into(),
                    );
                }
            }
            Some(InputMode::FocusedKeyboard) => {
                if button || scroll {
                    return Err("focused keyboard mode cannot emit pointer input".into());
                }
                if keyboard {
                    state.focused_keyboard_device().map(drop)?;
                }
            }
            None => return Err("EIS input mode was not prepared".into()),
        }
        Ok(())
    }

    pub fn resolve_keysyms(&self, keysyms: &[u32]) -> Result<Vec<ResolvedKey>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        let mode = state
            .mode
            .ok_or_else(|| "EIS input mode was not prepared".to_owned())?;
        let id = select_keyboard_for_mode(
            mode,
            || {
                let pointer_id = self.pointer_device_for_action(&state)?.pointer_id;
                state.keyboard_device(pointer_id)?.ok_or_else(|| {
                    "no resumed and synchronized EIS keyboard is available".to_owned()
                })
            },
            || state.focused_keyboard_device(),
        )?;
        Self::resolve_keysyms_with_device(&state, id, keysyms)
    }

    fn resolve_keysyms_with_device(
        state: &EisState,
        id: u64,
        keysyms: &[u32],
    ) -> Result<Vec<ResolvedKey>, String> {
        let device = &state.devices[&id];
        let keymap = parse_keymap(
            device
                .keymap
                .as_ref()
                .ok_or("EIS keyboard lost its keymap")?
                .clone(),
        )?;
        let mut xkb_state = xkb::State::new(&keymap);
        let (depressed, latched, locked, group) = device
            .modifiers
            .ok_or("EIS keyboard modifiers are not synchronized")?;
        validate_physical_modifiers(&keymap, (depressed, latched, locked, group))?;
        xkb_state.update_mask(depressed, latched, locked, 0, 0, group);
        let active = xkb_state.serialize_mods(xkb::STATE_MODS_EFFECTIVE);
        keysyms
            .iter()
            .map(|&keysym| {
                let target = xkb::Keysym::new(keysym);
                for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                    let keycode = xkb::Keycode::new(raw);
                    if xkb_state.key_get_one_sym(keycode) == target {
                        return Ok(ResolvedKey {
                            device_id: id,
                            resume_generation: device.resume_generation,
                            keycode: raw
                                .checked_sub(8)
                                .ok_or("XKB keycode is below the evdev offset")?,
                            modifiers: [None; 4],
                        });
                    }
                }
                for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                    let keycode = xkb::Keycode::new(raw);
                    for level in 0..keymap.num_levels_for_key(keycode, group) {
                        if !keymap
                            .key_get_syms_by_level(keycode, group, level)
                            .contains(&target)
                        {
                            continue;
                        }
                        let mut masks = [0; 16];
                        let count =
                            keymap.key_get_mods_for_level(keycode, group, level, &mut masks);
                        let required = masks[..count]
                            .iter()
                            .copied()
                            .filter_map(|mask| {
                                let added = mask & !active;
                                let mut candidate = xkb::State::new(&keymap);
                                candidate.update_mask(
                                    depressed | added,
                                    latched,
                                    locked,
                                    0,
                                    0,
                                    group,
                                );
                                (candidate.key_get_one_sym(keycode) == target).then_some(added)
                            })
                            .next()
                            .ok_or_else(|| {
                                format!(
                                    "active EIS keymap has no safe modifier mask for keysym 0x{keysym:x}"
                                )
                            })?;
                        return Ok(ResolvedKey {
                            device_id: id,
                            resume_generation: device.resume_generation,
                            keycode: raw
                                .checked_sub(8)
                                .ok_or("XKB keycode is below the evdev offset")?,
                            modifiers: modifier_keys(&keymap, required)?,
                        });
                    }
                }
                Err(format!(
                    "active EIS keymap cannot represent keysym 0x{keysym:x}"
                ))
            })
            .collect()
    }

    fn selected_device(
        &self,
        state: &EisState,
        mode: InputMode,
        event: &InputEvent,
    ) -> Result<u64, String> {
        match (mode, event) {
            (InputMode::Pointer, InputEvent::Absolute { .. } | InputEvent::Button { .. }) => self
                .pointer_device_for_action(state)
                .map(|binding| binding.pointer_id),
            (InputMode::Pointer, InputEvent::ScrollDiscrete { .. }) => {
                let pointer = self.pointer_device_for_action(state)?.pointer_id;
                state
                    .scroll_device(pointer)?
                    .ok_or_else(|| "EIS scroll device is no longer resumed".into())
            }
            (InputMode::FocusedKeyboard, InputEvent::Absolute { .. })
            | (InputMode::FocusedKeyboard, InputEvent::Button { .. })
            | (InputMode::FocusedKeyboard, InputEvent::ScrollDiscrete { .. }) => {
                Err("focused keyboard mode cannot emit pointer input".into())
            }
            (_, InputEvent::Keycode { key, .. }) => {
                let current = select_keyboard_for_mode(
                    mode,
                    || {
                        let pointer = self.pointer_device_for_action(state)?.pointer_id;
                        state.keyboard_device(pointer)?.ok_or_else(|| {
                            "synchronized EIS keyboard is no longer resumed".to_owned()
                        })
                    },
                    || state.focused_keyboard_device(),
                )?;
                let device = state
                    .devices
                    .get(&key.device_id)
                    .ok_or("resolved EIS keyboard disappeared")?;
                validate_key_binding(current, device.resume_generation, *key)?;
                Ok(current)
            }
        }
    }

    fn begin_inner_for_mode(&self, mode: InputMode) -> Result<(), String> {
        if self.session.is_closed() {
            return Err("portal RemoteDesktop Session.Closed".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        if let Some(error) = &state.terminal {
            return Err(error.clone());
        }
        let connection = state
            .connection
            .clone()
            .ok_or("EIS connection is not ready")?;
        match mode {
            InputMode::Pointer => {
                let binding = self.pointer_device_for_action(&state)?;
                let device = state
                    .devices
                    .get_mut(&binding.pointer_id)
                    .ok_or("selected EIS pointer disappeared")?;
                if !device.emulating {
                    device
                        .device
                        .device()
                        .start_emulating(connection.serial(), device.sequence);
                    device.sequence = device.sequence.wrapping_add(1);
                    device.emulating = true;
                }
            }
            InputMode::FocusedKeyboard => {
                state.focused_keyboard_device().map(drop)?;
            }
        }
        connection
            .flush()
            .map_err(|error| format!("cannot start EIS emulation: {error}"))
    }

    fn emit_inner_for_mode(&self, mode: InputMode, event: InputEvent) -> Result<(), String> {
        validate_event(&event)?;
        if self.session.is_closed() {
            return Err("portal RemoteDesktop Session.Closed".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        if let Some(error) = &state.terminal {
            return Err(error.clone());
        }
        let connection = state
            .connection
            .clone()
            .ok_or("EIS connection is not ready")?;
        let device_id = self.selected_device(&state, mode, &event)?;
        let device = state
            .devices
            .get_mut(&device_id)
            .ok_or("selected EIS device disappeared")?;
        if !device.emulating {
            if !matches!(
                event,
                InputEvent::ScrollDiscrete { .. } | InputEvent::Keycode { .. }
            ) {
                return Err("selected EIS device is not in an emulation sequence".into());
            }
            if matches!(event, InputEvent::Keycode { .. }) {
                ensure_safe_physical_modifiers(device)?;
            }
            device
                .device
                .device()
                .start_emulating(connection.serial(), device.sequence);
            device.sequence = device.sequence.wrapping_add(1);
            device.emulating = true;
        }
        match event {
            InputEvent::Absolute { x, y } => device
                .device
                .interface::<ei::PointerAbsolute>()
                .ok_or("EIS device lost absolute pointer")?
                .motion_absolute(f32_value(x)?, f32_value(y)?),
            InputEvent::Button { code, pressed } => device
                .device
                .interface::<ei::Button>()
                .ok_or("EIS device lost button capability")?
                .button(
                    code,
                    if pressed {
                        ei::button::ButtonState::Press
                    } else {
                        ei::button::ButtonState::Released
                    },
                ),
            InputEvent::ScrollDiscrete { x, y } => {
                let scroll = device
                    .device
                    .interface::<ei::Scroll>()
                    .ok_or("EIS device lost scroll capability")?;
                scroll.scroll_discrete(x, y);
                device
                    .device
                    .device()
                    .frame(connection.serial(), monotonic_microseconds());
                scroll.scroll_stop(u32::from(x != 0), u32::from(y != 0), 0);
            }
            InputEvent::Keycode { key, pressed } => device
                .device
                .interface::<ei::Keyboard>()
                .ok_or("EIS device lost keyboard capability")?
                .key(
                    key.keycode,
                    if pressed {
                        ei::keyboard::KeyState::Press
                    } else {
                        ei::keyboard::KeyState::Released
                    },
                ),
        }
        device
            .device
            .device()
            .frame(connection.serial(), monotonic_microseconds());
        connection
            .flush()
            .map_err(|error| format!("cannot flush EIS event: {error}"))
    }

    fn end_inner(&self) -> Result<Option<u64>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        let connection = state
            .connection
            .clone()
            .ok_or("EIS connection is not ready")?;
        let active = state
            .devices
            .iter()
            .filter_map(|(&id, device)| device.emulating.then_some(id))
            .collect::<Vec<_>>();
        for id in &active {
            let device = state
                .devices
                .get(id)
                .ok_or("emulating EIS device disappeared")?;
            device.device.device().stop_emulating(connection.serial());
        }
        connection
            .flush()
            .map_err(|error| format!("cannot stop EIS emulation: {error}"))?;
        let mut keyboard = None;
        for id in active {
            if let Some(device) = state.devices.get_mut(&id) {
                device.emulating = false;
                if device.device.interface::<ei::Keyboard>().is_some() {
                    device.modifiers_synced = false;
                    keyboard = Some(id);
                }
            }
        }
        Ok(keyboard)
    }

    fn current_mode(&self) -> Result<InputMode, String> {
        self.state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?
            .mode
            .ok_or_else(|| "EIS input mode was not prepared".to_owned())
    }

    fn transaction_mode(&self) -> Result<InputMode, String> {
        self.cleanup
            .lock()
            .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
            .mode
            .ok_or_else(|| "EIS transaction mode was not started".to_owned())
    }

    async fn synchronize(&self, keyboard_id: Option<u64>) -> Result<(), String> {
        let (response, result) = oneshot::channel();
        self.sync_requests
            .send(SyncRequest {
                keyboard_id,
                response,
            })
            .map_err(|_| "EIS event thread is unavailable for synchronization".to_owned())?;
        tokio::time::timeout(SYNC_TIMEOUT, result)
            .await
            .map_err(|_| "timed out synchronizing the EIS transaction".to_owned())?
            .map_err(|_| "EIS synchronization callback was dropped".to_owned())?
    }
}

impl InputBackend for ReisInputBackend {
    fn begin_sequence(&self) -> InputFuture<'_> {
        Box::pin(async move {
            let _serial = self.serial.lock().await;
            let mode = self.current_mode();
            let cleanup_mode = mode.as_ref().ok().copied();
            let result = match mode {
                Ok(mode) => self.begin_inner_for_mode(mode),
                Err(error) => Err(error),
            };
            let sequence_open = self
                .state
                .lock()
                .map(|state| state.devices.values().any(|device| device.emulating))
                .unwrap_or(true);
            if result.is_ok() || sequence_open {
                self.cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                    .sequence_pending = true;
                self.cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                    .mode = cleanup_mode;
            }
            result
        })
    }

    fn emit(&self, event: InputEvent) -> InputFuture<'_> {
        Box::pin(async move {
            let _serial = self.serial.lock().await;
            let mode = self.transaction_mode()?;
            self.emit_inner_for_mode(mode, event)
        })
    }

    fn sync_barrier(&self) -> InputFuture<'_> {
        Box::pin(async move {
            let _serial = self.serial.lock().await;
            self.synchronize(None).await
        })
    }

    fn queue_release(&self, held: Vec<HeldInput>) {
        match self.cleanup.lock() {
            Ok(mut cleanup) => cleanup.held.extend(held),
            Err(_) => {
                eprintln!("computer-use-mcp: EIS cleanup mutex poisoned; invalidating session");
                self.session.invalidate("EIS cleanup mutex poisoned");
            }
        }
    }

    fn cleanup_barrier(&self) -> InputFuture<'_> {
        Box::pin(async move {
            let _serial = self.serial.lock().await;
            let (held, sequence_pending, mode) = {
                let cleanup = self
                    .cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?;
                (cleanup.held.clone(), cleanup.sequence_pending, cleanup.mode)
            };
            if held.is_empty() && !sequence_pending {
                return Ok(());
            }
            let mode = mode.ok_or("EIS cleanup has no transaction mode")?;
            if !sequence_pending {
                self.begin_inner_for_mode(mode)?;
                self.cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                    .sequence_pending = true;
            }
            let mut first = None;
            for input in held {
                match self.emit_inner_for_mode(mode, input.release_event()) {
                    Ok(()) => {
                        if let Ok(mut cleanup) = self.cleanup.lock()
                            && let Some(index) = cleanup
                                .held
                                .iter()
                                .rposition(|candidate| *candidate == input)
                        {
                            cleanup.held.remove(index);
                        }
                    }
                    Err(error) => {
                        first.get_or_insert(error);
                    }
                }
            }
            match self.end_inner() {
                Ok(keyboard) => {
                    if let Err(error) = self.synchronize(keyboard).await {
                        first.get_or_insert(error);
                    }
                    self.cleanup
                        .lock()
                        .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                        .sequence_pending = false;
                    self.cleanup
                        .lock()
                        .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                        .mode = None;
                    if first.is_none() {
                        self.state
                            .lock()
                            .map_err(|_| "EIS state mutex poisoned".to_owned())?
                            .mode = None;
                    }
                }
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
            if first.is_some() {
                self.session
                    .invalidate("EIS could not confirm held-input cleanup");
            }
            first.map_or(Ok(()), Err)
        })
    }
}

impl Drop for ReisInputBackend {
    fn drop(&mut self) {
        let Ok(mut thread) = self.thread.lock() else {
            eprintln!("computer-use-mcp: EIS thread mutex poisoned during shutdown");
            self.session.invalidate("EIS thread mutex poisoned");
            return;
        };
        thread.stopping.store(true, Ordering::Release);
        if let Some(socket) = thread.shutdown.take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        let Some(handle) = thread.handle.take() else {
            return;
        };
        if thread.done.recv_timeout(Duration::from_secs(1)).is_ok() {
            let _ = handle.join();
        } else {
            eprintln!(
                "computer-use-mcp: EIS event thread did not stop within one second; detaching it"
            );
        }
    }
}

fn run_eis(
    socket: UnixStream,
    state: Arc<Mutex<EisState>>,
    ready: Arc<Notify>,
    session: Arc<PortalSessionLease>,
    stopping: Arc<AtomicBool>,
    sync_requests: mpsc::UnboundedReceiver<SyncRequest>,
) {
    let result = run_eis_inner(
        socket,
        Arc::clone(&state),
        Arc::clone(&ready),
        sync_requests,
    );
    let error = result
        .err()
        .unwrap_or_else(|| "EIS event thread stopped".into());
    if let Ok(mut state) = state.lock() {
        state.terminal = Some(error.clone());
    }
    ready.notify_one();
    if !stopping.load(Ordering::Acquire) {
        eprintln!("computer-use-mcp: {error}");
        session.invalidate("EIS connection terminated");
    }
}

fn run_eis_inner(
    socket: UnixStream,
    state: Arc<Mutex<EisState>>,
    ready: Arc<Notify>,
    mut sync_requests: mpsc::UnboundedReceiver<SyncRequest>,
) -> Result<(), String> {
    let context =
        ei::Context::new(socket).map_err(|error| format!("cannot create EIS context: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|error| format!("cannot create EIS runtime: {error}"))?;
    runtime.block_on(async move {
        let mut wire_events = reis::tokio::EiEventStream::new(context.clone())
            .map_err(|error| format!("cannot monitor EIS socket: {error}"))?;
        let handshake = reis::tokio::ei_handshake(
            &mut wire_events,
            "computer-use-mcp",
            ei::handshake::ContextType::Sender,
        )
        .await
        .map_err(|error| format!("EIS handshake failed: {error}"))?;
        let mut converter = EiEventConverter::new(&context, handshake);
        let connection = converter.connection().clone();
        state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?
            .connection = Some(connection.clone());

        loop {
            let result = tokio::select! {
                result = wire_events.next() => result.ok_or("EIS socket reached EOF")?,
                request = sync_requests.recv() => {
                    let request = request.ok_or("EIS synchronization channel closed")?;
                    queue_sync(
                        request.keyboard_id,
                        connection.clone(),
                        &mut converter,
                        Arc::clone(&state),
                        Arc::clone(&ready),
                        Some(request.response),
                    )?;
                    connection
                        .flush()
                        .map_err(|error| format!("cannot synchronize EIS transaction: {error}"))?;
                    continue;
                }
            };
            let wire_event =
                match result.map_err(|error| format!("cannot read EIS event: {error}"))? {
                    PendingRequestResult::Request(event) => event,
                    PendingRequestResult::ParseError(error) => {
                        return Err(format!("cannot parse EIS event: {error}"));
                    }
                    PendingRequestResult::InvalidObject(id) => {
                        return Err(format!("EIS event referenced invalid object {id}"));
                    }
                };
            if let ei::Event::Connection(
                _,
                ei::connection::Event::InvalidObject {
                    last_serial,
                    invalid_id,
                },
            ) = &wire_event
            {
                return Err(format!(
                    "EIS rejected object {invalid_id} after serial {last_serial}"
                ));
            }
            converter
                .handle_event(wire_event)
                .map_err(|error| format!("EIS protocol failed: {error}"))?;
            while let Some(event) = converter.next_event() {
                handle_event(event, &connection, &mut converter, &state, &ready)?;
            }
        }
    })
}

fn handle_event(
    event: EiEvent,
    connection: &reis::event::Connection,
    converter: &mut EiEventConverter,
    state: &Arc<Mutex<EisState>>,
    ready: &Arc<Notify>,
) -> Result<(), String> {
    match event {
        EiEvent::SeatAdded(event) => {
            event.seat.bind_capabilities(
                DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            );
            connection
                .flush()
                .map_err(|error| format!("cannot bind EIS seat: {error}"))?;
        }
        EiEvent::DeviceAdded(event) => {
            if event.device.device().version() >= 3 {
                event.device.device().ready();
            }
            let id = device_id(&event.device);
            let keymap = if event.device.interface::<ei::Keyboard>().is_some() {
                match event
                    .device
                    .keymap()
                    .ok_or_else(|| "device did not provide an XKB keymap".to_owned())
                    .and_then(read_keymap)
                    .and_then(|text| parse_keymap(text.clone()).map(|_| text))
                {
                    Ok(text) => Some(text),
                    Err(error) => {
                        eprintln!("computer-use-mcp: ignoring unusable EIS keyboard {id}: {error}");
                        None
                    }
                }
            } else {
                None
            };
            state
                .lock()
                .map_err(|_| "EIS state mutex poisoned".to_owned())?
                .devices
                .insert(
                    id,
                    DeviceState {
                        device: event.device,
                        keymap,
                        resumed: false,
                        modifiers_synced: false,
                        resume_generation: 0,
                        sequence: 1,
                        modifiers: None,
                        emulating: false,
                    },
                );
            connection
                .flush()
                .map_err(|error| format!("cannot ready EIS device: {error}"))?;
        }
        EiEvent::DeviceResumed(event) => {
            let id = device_id(&event.device);
            let synchronize_keyboard = {
                let mut state = state
                    .lock()
                    .map_err(|_| "EIS state mutex poisoned".to_owned())?;
                match state.devices.get_mut(&id) {
                    Some(device) => {
                        device.resumed = true;
                        device.resume_generation = device.resume_generation.wrapping_add(1);
                        device.modifiers_synced = false;
                        let usable_keyboard = device.is_usable_keyboard();
                        // The EI protocol requires senders to assume all modifiers are
                        // lifted after resume; only nonzero state must be reported.
                        device.modifiers = usable_keyboard.then_some((0, 0, 0, 0));
                        usable_keyboard
                    }
                    None => false,
                }
            };
            if synchronize_keyboard {
                queue_sync(
                    Some(id),
                    connection.clone(),
                    converter,
                    Arc::clone(state),
                    Arc::clone(ready),
                    None,
                )?;
                connection
                    .flush()
                    .map_err(|error| format!("cannot synchronize EIS keyboard: {error}"))?;
            }
            ready.notify_one();
        }
        EiEvent::DevicePaused(event) => {
            let id = device_id(&event.device);
            if let Some(device) = state
                .lock()
                .map_err(|_| "EIS state mutex poisoned".to_owned())?
                .devices
                .get_mut(&id)
            {
                device.resumed = false;
                device.modifiers = None;
                device.modifiers_synced = false;
                device.emulating = false;
            }
        }
        EiEvent::KeyboardModifiers(event) => {
            let id = device_id(&event.device);
            if let Some(device) = state
                .lock()
                .map_err(|_| "EIS state mutex poisoned".to_owned())?
                .devices
                .get_mut(&id)
            {
                device.modifiers =
                    Some((event.depressed, event.latched, event.locked, event.group));
            }
            ready.notify_one();
        }
        EiEvent::DeviceRemoved(event) => {
            state
                .lock()
                .map_err(|_| "EIS state mutex poisoned".to_owned())?
                .devices
                .remove(&device_id(&event.device));
        }
        EiEvent::SeatRemoved(event) => {
            state
                .lock()
                .map_err(|_| "EIS state mutex poisoned".to_owned())?
                .devices
                .retain(|_, device| device.device.seat() != &event.seat);
            ready.notify_one();
        }
        EiEvent::Disconnected(event) => {
            return Err(format!(
                "EIS disconnected: {:?}: {}",
                event.reason,
                event.explanation.unwrap_or_default()
            ));
        }
        _ => {}
    }
    Ok(())
}

fn queue_sync(
    device_id: Option<u64>,
    connection: reis::event::Connection,
    converter: &mut EiEventConverter,
    state: Arc<Mutex<EisState>>,
    ready: Arc<Notify>,
    response: Option<oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    let resume_generation = if let Some(device_id) = device_id {
        let mut state = state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        let device = state
            .devices
            .get_mut(&device_id)
            .ok_or("EIS keyboard disappeared before synchronization")?;
        if !device.resumed || !device.is_usable_keyboard() {
            return Err("EIS keyboard paused before synchronization".into());
        }
        device.modifiers_synced = false;
        Some(device.resume_generation)
    } else {
        None
    };
    let callback = connection.connection().sync(1);
    converter.add_callback_handler(callback, move |_| {
        let result = match (device_id, resume_generation) {
            (Some(device_id), Some(resume_generation)) => match state.lock() {
                Ok(mut state) => match state.devices.get_mut(&device_id) {
                    Some(device)
                        if device.resumed
                            && device.resume_generation == resume_generation
                            && device.modifiers.is_some() =>
                    {
                        device.modifiers_synced = true;
                        Ok(())
                    }
                    _ => Err("EIS keyboard changed during synchronization".into()),
                },
                Err(_) => Err("EIS state mutex poisoned".into()),
            },
            (None, None) => Ok(()),
            _ => Err("EIS synchronization state is inconsistent".into()),
        };
        if let Some(response) = response {
            let _ = response.send(result);
        }
        ready.notify_one();
    });
    Ok(())
}

fn read_keymap(keymap: &reis::event::Keymap) -> Result<String, String> {
    if keymap.type_ != ei::keyboard::KeymapType::Xkb {
        return Err("EIS supplied an unsupported keymap type".into());
    }
    let fd = keymap
        .fd
        .try_clone()
        .map_err(|error| format!("cannot clone EIS keymap fd: {error}"))?;
    let mut file = std::fs::File::from(fd);
    file.rewind()
        .map_err(|error| format!("cannot rewind EIS keymap: {error}"))?;
    let mut text = String::new();
    file.take(u64::from(keymap.size))
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read EIS keymap: {error}"))?;
    Ok(text)
}

fn parse_keymap(text: String) -> Result<xkb::Keymap, String> {
    xkb::Keymap::new_from_string(&xkb::Context::new(0), text, xkb::KEYMAP_FORMAT_TEXT_V1, 0)
        .ok_or_else(|| "cannot parse EIS XKB keymap".into())
}

fn ensure_safe_physical_modifiers(device: &DeviceState) -> Result<(), String> {
    let keymap = parse_keymap(
        device
            .keymap
            .as_ref()
            .ok_or("EIS keyboard lost its keymap")?
            .clone(),
    )?;
    let modifiers = device
        .modifiers
        .ok_or("EIS keyboard modifiers are not synchronized")?;
    validate_physical_modifiers(&keymap, modifiers)
}

fn validate_physical_modifiers(
    keymap: &xkb::Keymap,
    (depressed, latched, locked, group): (u32, u32, u32, u32),
) -> Result<(), String> {
    if latched != 0 {
        return Err(
            "a physical latched modifier is active; refusing generated keyboard input".into(),
        );
    }
    let mut state = xkb::State::new(keymap);
    state.update_mask(depressed, latched, locked, 0, 0, group);
    let active = state.serialize_mods(xkb::STATE_MODS_EFFECTIVE);
    let shortcuts = modifier_mask(
        keymap,
        &[xkb::MOD_NAME_CTRL, xkb::MOD_NAME_ALT, xkb::MOD_NAME_LOGO],
    );
    if active & shortcuts != 0 {
        return Err(
            "physical Ctrl, Alt, or Super is active; refusing generated keyboard input".into(),
        );
    }
    Ok(())
}

fn find_key(keymap: &xkb::Keymap, symbol: xkb::Keysym) -> Result<u32, String> {
    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        if keymap
            .key_get_syms_by_level(xkb::Keycode::new(raw), 0, 0)
            .contains(&symbol)
        {
            return raw
                .checked_sub(8)
                .ok_or_else(|| "XKB keycode is below the evdev offset".into());
        }
    }
    Err(format!("EIS keymap has no key for {symbol:?}"))
}

fn modifier_keys(keymap: &xkb::Keymap, mask: xkb::ModMask) -> Result<[Option<u32>; 4], String> {
    let mut keys = [None; 4];
    for (slot, (name, symbol)) in [
        (xkb::MOD_NAME_SHIFT, xkb::Keysym::Shift_L),
        (xkb::MOD_NAME_CTRL, xkb::Keysym::Control_L),
        (xkb::MOD_NAME_ALT, xkb::Keysym::Alt_L),
        (xkb::MOD_NAME_LOGO, xkb::Keysym::Super_L),
    ]
    .into_iter()
    .enumerate()
    {
        let index = keymap.mod_get_index(name);
        if index != xkb::MOD_INVALID && mask & (1_u32 << index) != 0 {
            keys[slot] = Some(find_key(keymap, symbol)?);
        }
    }
    let known = [
        xkb::MOD_NAME_SHIFT,
        xkb::MOD_NAME_CTRL,
        xkb::MOD_NAME_ALT,
        xkb::MOD_NAME_LOGO,
    ]
    .into_iter()
    .map(|name| keymap.mod_get_index(name))
    .filter(|index| *index != xkb::MOD_INVALID)
    .fold(0, |known, index| known | (1_u32 << index));
    if mask & !known != 0 {
        return Err("EIS keymap requires an unsupported modifier combination".into());
    }
    Ok(keys)
}

fn modifier_mask(keymap: &xkb::Keymap, names: &[&str]) -> xkb::ModMask {
    names
        .iter()
        .map(|name| keymap.mod_get_index(name))
        .filter(|index| *index != xkb::MOD_INVALID)
        .fold(0, |mask, index| mask | (1_u32 << index))
}

fn validate_event(event: &InputEvent) -> Result<(), String> {
    match event {
        InputEvent::Absolute { x, y } => {
            f32_value(*x)?;
            f32_value(*y)?;
        }
        InputEvent::ScrollDiscrete { x, y } => {
            if *x == 0 && *y == 0 {
                return Err("EIS discrete scroll delta must not be zero".into());
            }
        }
        InputEvent::Button { .. } | InputEvent::Keycode { .. } => {}
    }
    Ok(())
}

fn select_keyboard_for_mode<P, F>(mode: InputMode, pointer: P, focused: F) -> Result<u64, String>
where
    P: FnOnce() -> Result<u64, String>,
    F: FnOnce() -> Result<u64, String>,
{
    match mode {
        InputMode::Pointer => pointer(),
        InputMode::FocusedKeyboard => focused(),
    }
}

fn validate_key_binding(
    current_device_id: u64,
    current_resume_generation: u64,
    key: KeyboardKey,
) -> Result<(), String> {
    if current_device_id != key.device_id || current_resume_generation != key.resume_generation {
        return Err(
            "EIS keyboard changed after key resolution; inspect fresh state and retry".into(),
        );
    }
    Ok(())
}

fn f32_value(value: f64) -> Result<f32, String> {
    let value = value as f32;
    value
        .is_finite()
        .then_some(value)
        .ok_or_else(|| "EIS coordinate exceeds f32 range".into())
}

fn monotonic_microseconds() -> u64 {
    let time = clock_gettime(ClockId::Monotonic);
    let seconds = u64::try_from(time.tv_sec).unwrap_or_default();
    let nanos = u64::try_from(time.tv_nsec).unwrap_or_default();
    seconds
        .saturating_mul(1_000_000)
        .saturating_add(nanos / 1_000)
}

fn device_id(device: &Device) -> u64 {
    device.device().as_object().id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_attempt_invalidates_the_portal_session() {
        let (session, _) = PortalSessionLease::for_test("/session/eis", 1);
        let attempt = EisAttemptGuard::new(Arc::clone(&session)).unwrap();
        drop(attempt);
        assert!(session.is_closed());
        assert!(session.begin_eis_attempt().is_err());
    }

    #[test]
    fn monotonic_timestamp_uses_the_system_clock_epoch() {
        let first = monotonic_microseconds();
        let second = monotonic_microseconds();
        assert!(first > 0);
        assert!(second >= first);
    }

    #[test]
    fn resolved_keys_are_bound_to_device_and_resume_generation() {
        let key = KeyboardKey {
            device_id: 4,
            resume_generation: 7,
            keycode: 30,
        };
        assert!(validate_key_binding(4, 7, key).is_ok());
        assert!(validate_key_binding(5, 7, key).is_err());
        assert!(validate_key_binding(4, 8, key).is_err());
    }

    #[test]
    fn transaction_mode_does_not_fall_back_between_authorities() {
        let pointer_stale = || Err("pointer binding is stale".to_owned());
        let focused_ready = || Ok(22);
        assert!(
            select_keyboard_for_mode(InputMode::Pointer, pointer_stale, focused_ready).is_err()
        );

        let pointer_stale = || panic!("focused mode must not inspect the pointer authority");
        assert_eq!(
            select_keyboard_for_mode(InputMode::FocusedKeyboard, pointer_stale, || Ok(22)),
            Ok(22)
        );
    }

    #[test]
    fn switching_transaction_modes_clears_the_other_authority() {
        let mut state = EisState::new();
        state.bind_pointer_mode(EisBinding {
            pointer_id: 4,
            resume_generation: 1,
            region: EisRegion {
                position: (0, 0),
                size: (100, 100),
                mapping_id: None,
            },
            union_sources: Vec::new(),
        });
        assert_eq!(state.mode, Some(InputMode::Pointer));
        assert!(state.binding.is_some());
        assert!(state.focused_keyboard.is_none());

        state.bind_focused_keyboard_mode(FocusedKeyboardBinding {
            device_id: 8,
            resume_generation: 3,
        });
        assert_eq!(state.mode, Some(InputMode::FocusedKeyboard));
        assert!(state.binding.is_none());
        assert_eq!(
            state.focused_keyboard,
            Some(FocusedKeyboardBinding {
                device_id: 8,
                resume_generation: 3,
            })
        );
    }
}
