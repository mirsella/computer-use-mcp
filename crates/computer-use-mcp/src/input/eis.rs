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
    backend::{HeldInput, InputBackend, InputEvent, InputFuture, KeyboardKey},
    coordinates::{EisRegion, EisRoute},
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
}

struct EisState {
    connection: Option<reis::event::Connection>,
    devices: HashMap<u64, DeviceState>,
    terminal: Option<String>,
    binding: Option<EisBinding>,
}

impl EisState {
    fn new() -> Self {
        Self {
            connection: None,
            devices: HashMap::new(),
            terminal: None,
            binding: None,
        }
    }

    fn pointer_device(&self, route: &EisRoute) -> Result<Option<EisBinding>, String> {
        let mut matched = None;
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
                    let candidate = EisBinding {
                        pointer_id: id,
                        resume_generation: state.resume_generation,
                        region,
                    };
                    if matched.replace(candidate).is_some() {
                        return Err("multiple resumed EIS regions match the selected monitor stream; refusing ambiguous input".into());
                    }
                }
            }
        }
        Ok(matched)
    }

    fn scroll_device(&self, pointer_device: u64) -> Result<Option<u64>, String> {
        let pointer = self
            .devices
            .get(&pointer_device)
            .ok_or("selected EIS pointer disappeared")?;
        if pointer.device.interface::<ei::Scroll>().is_some() {
            return Ok(Some(pointer_device));
        }
        let matches = self
            .devices
            .iter()
            .filter(|(_, state)| {
                state.resumed
                    && state.device.seat() == pointer.device.seat()
                    && state.device.interface::<ei::Scroll>().is_some()
            })
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [id] => Ok(Some(*id)),
            [] => Ok(None),
            many => Err(format!(
                "{} resumed EIS scroll devices share the selected pointer seat; refusing ambiguous input",
                many.len()
            )),
        }
    }

    fn keyboard_device(&self, pointer_device: u64) -> Result<Option<u64>, String> {
        let pointer_seat = self
            .devices
            .get(&pointer_device)
            .ok_or("selected EIS pointer disappeared")?
            .device
            .seat();
        let devices = self
            .devices
            .iter()
            .filter(|(_, state)| {
                state.resumed
                    && state.modifiers_synced
                    && state.modifiers.is_some()
                    && state.is_usable_keyboard()
                    && state.device.seat() == pointer_seat
            })
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        match devices.as_slice() {
            [id] => Ok(Some(*id)),
            [] => Ok(None),
            many => Err(format!(
                "{} resumed and synchronized EIS keyboards are available; refusing ambiguous input",
                many.len()
            )),
        }
    }

    fn keyboard_diagnostics(&self, route: &EisRoute) -> Result<String, String> {
        let pointer_id = self
            .pointer_device(route)?
            .ok_or("exact EIS pointer region is not resumed")?
            .pointer_id;
        let pointer = self
            .devices
            .get(&pointer_id)
            .ok_or("selected EIS pointer disappeared")?;
        let details = self
            .devices
            .iter()
            .filter(|(_, state)| state.device.interface::<ei::Keyboard>().is_some())
            .map(|(id, state)| {
                format!(
                    "device {id}: same_seat={}, resumed={}, keymap={}, modifiers={}, synchronized={}",
                    state.device.seat() == pointer.device.seat(),
                    state.resumed,
                    state.keymap.is_some(),
                    state.modifiers.is_some(),
                    state.modifiers_synced
                )
            })
            .collect::<Vec<_>>();
        Ok(if details.is_empty() {
            "no EIS keyboard device was advertised".into()
        } else {
            details.join("; ")
        })
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
}

#[derive(Clone, PartialEq, Eq)]
struct EisBinding {
    pointer_id: u64,
    resume_generation: u64,
    region: EisRegion,
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
        tokio::time::timeout(READY_TIMEOUT, backend.wait_ready(false))
            .await
            .map_err(|_| "timed out waiting for the exact EIS monitor device".to_owned())??;
        attempt.complete()?;
        Ok(backend)
    }

    async fn wait_ready(&self, keyboard_required: bool) -> Result<EisRegion, String> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "EIS state mutex poisoned".to_owned())?;
                if let Some(error) = &state.terminal {
                    return Err(error.clone());
                }
                if let Some(binding) = state.pointer_device(&self.route)? {
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
                        state.binding = Some(binding);
                        return Ok(region);
                    }
                }
            }
            self.ready.notified().await;
        }
    }

    pub async fn wait_for_action(&self, keyboard_required: bool) -> Result<EisRegion, String> {
        tokio::time::timeout(READY_TIMEOUT, self.wait_ready(keyboard_required))
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

    pub fn region(&self) -> Result<EisRegion, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        if let Some(error) = &state.terminal {
            return Err(error.clone());
        }
        Ok(self.pointer_device_for_action(&state)?.region)
    }

    fn pointer_device_for_action(&self, state: &EisState) -> Result<EisBinding, String> {
        let current = state
            .pointer_device(&self.route)?
            .ok_or_else(|| "exact EIS pointer region is no longer resumed".to_owned())?;
        let bound = state
            .binding
            .as_ref()
            .ok_or("EIS monitor region was not bound during input preparation")?;
        if bound != &current {
            return Err("selected EIS region changed before input dispatch".into());
        }
        Ok(current)
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
        Ok(())
    }

    pub fn resolve_keysyms(&self, keysyms: &[u32]) -> Result<Vec<ResolvedKey>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "EIS state mutex poisoned".to_owned())?;
        let pointer_id = self.pointer_device_for_action(&state)?.pointer_id;
        let id = state
            .keyboard_device(pointer_id)?
            .ok_or_else(|| "no resumed and synchronized EIS keyboard is available".to_owned())?;
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

    fn selected_device(&self, state: &EisState, event: &InputEvent) -> Result<u64, String> {
        match event {
            InputEvent::Absolute { .. } | InputEvent::Button { .. } => self
                .pointer_device_for_action(state)
                .map(|binding| binding.pointer_id),
            InputEvent::ScrollDiscrete { .. } => {
                let pointer = self.pointer_device_for_action(state)?.pointer_id;
                state
                    .scroll_device(pointer)?
                    .ok_or_else(|| "EIS scroll device is no longer resumed".into())
            }
            InputEvent::Keycode { key, .. } => {
                let pointer = self.pointer_device_for_action(state)?.pointer_id;
                let current = state
                    .keyboard_device(pointer)?
                    .ok_or("synchronized EIS keyboard is no longer resumed")?;
                let device = state
                    .devices
                    .get(&key.device_id)
                    .ok_or("resolved EIS keyboard disappeared")?;
                validate_key_binding(current, device.resume_generation, *key)?;
                Ok(current)
            }
        }
    }

    fn begin_inner(&self) -> Result<(), String> {
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
        let pointer_id = self.pointer_device_for_action(&state)?.pointer_id;
        let device = state
            .devices
            .get_mut(&pointer_id)
            .ok_or("selected EIS pointer disappeared")?;
        if !device.emulating {
            device
                .device
                .device()
                .start_emulating(connection.serial(), device.sequence);
            device.sequence = device.sequence.wrapping_add(1);
            device.emulating = true;
        }
        connection
            .flush()
            .map_err(|error| format!("cannot start EIS emulation: {error}"))
    }

    fn emit_inner(&self, event: InputEvent) -> Result<(), String> {
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
        let device_id = self.selected_device(&state, &event)?;
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
            let result = self.begin_inner();
            let sequence_open = self
                .state
                .lock()
                .map(|state| state.devices.values().any(|device| device.emulating))
                .unwrap_or(true);
            if sequence_open {
                self.cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                    .sequence_pending = true;
            }
            result
        })
    }

    fn emit(&self, event: InputEvent) -> InputFuture<'_> {
        Box::pin(async move {
            let _serial = self.serial.lock().await;
            self.emit_inner(event)
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
            let (held, sequence_pending) = {
                let cleanup = self
                    .cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?;
                (cleanup.held.clone(), cleanup.sequence_pending)
            };
            if held.is_empty() && !sequence_pending {
                return Ok(());
            }
            if !sequence_pending {
                self.begin_inner()?;
                self.cleanup
                    .lock()
                    .map_err(|_| "EIS cleanup mutex poisoned".to_owned())?
                    .sequence_pending = true;
            }
            let mut first = None;
            for input in held {
                match self.emit_inner(input.release_event()) {
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
}
