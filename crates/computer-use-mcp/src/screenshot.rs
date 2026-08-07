use std::{future::Future, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::sync::Mutex;

use crate::{
    accessibility::{ObjectId, Snapshot},
    capture::{CaptureBackend, CaptureSession, FrameMetadata, OwnedFrame, PipeWireCapture},
    encoder,
    geometry::PixelRect,
    input::{
        GeneratedInputAction, backend::InputBackend, coordinates::ValidatedMapping,
        eis::ReisInputBackend, keyboard_input, pointer,
    },
    portal::{PortalBackend, PortalSessionLease, PortalStream, XdgPortalBackend},
    validation::{KeyboardFocus, PointerAction},
};

pub(crate) const SESSION_UNAVAILABLE: &str =
    "desktop session is unavailable; disable and re-enable the MCP to request KDE approval again";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotError(pub String);

impl std::fmt::Display for ScreenshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScreenshotError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareCapture {
    pub consent_interrupted_observation: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenshotMapping {
    pub app_pid: u32,
    pub app_identity: ObjectId,
    pub window_identity: ObjectId,
    pub accessibility_generation: u64,
    pub portal_session_identity: String,
    pub portal_session_generation: u64,
    pub stream: PortalStream,
    pub source: FrameMetadata,
    pub output_size: (u32, u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenshotObservation {
    pub png_base64: String,
    pub mapping: ScreenshotMapping,
}

pub trait ScreenshotProvider: Send + Sync + 'static {
    fn prepare(&self) -> impl Future<Output = Result<PrepareCapture, ScreenshotError>> + Send + '_;
    fn capture<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> impl Future<Output = Result<ScreenshotObservation, ScreenshotError>> + Send + 'a;
    fn prepare_input<'a>(
        &'a self,
        _snapshot: &'a Snapshot,
        _mapping: &'a ScreenshotMapping,
        _action: &'a GeneratedInputAction,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a {
        async { Ok(()) }
    }
    fn perform_input<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        mapping: &'a ScreenshotMapping,
        action: GeneratedInputAction,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;
    fn cleanup_input(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        async { Ok(()) }
    }
    fn shutdown_input(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        self.cleanup_input()
    }
}

#[derive(Debug, Default)]
pub struct NoScreenshots;

impl ScreenshotProvider for NoScreenshots {
    async fn prepare(&self) -> Result<PrepareCapture, ScreenshotError> {
        Ok(PrepareCapture {
            consent_interrupted_observation: false,
        })
    }

    async fn capture<'a>(
        &'a self,
        _snapshot: &'a Snapshot,
    ) -> Result<ScreenshotObservation, ScreenshotError> {
        Err(ScreenshotError("capture backend is not configured".into()))
    }

    async fn perform_input<'a>(
        &'a self,
        _snapshot: &'a Snapshot,
        _mapping: &'a ScreenshotMapping,
        _action: GeneratedInputAction,
    ) -> Result<(), String> {
        Err("generated input requires a live screenshot provider".into())
    }
}

pub type ProductionScreenshotCoordinator = ScreenshotCoordinator<XdgPortalBackend, PipeWireCapture>;

pub struct ScreenshotCoordinator<P, C> {
    portal: P,
    capture: C,
    state: Mutex<CaptureState>,
}

impl<P, C> std::fmt::Debug for ScreenshotCoordinator<P, C>
where
    P: std::fmt::Debug,
    C: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScreenshotCoordinator")
            .field("portal", &self.portal)
            .field("capture", &self.capture)
            .finish_non_exhaustive()
    }
}

impl Default for ProductionScreenshotCoordinator {
    fn default() -> Self {
        Self::new(XdgPortalBackend::default(), PipeWireCapture)
    }
}

impl<P, C> ScreenshotCoordinator<P, C> {
    pub fn new(portal: P, capture: C) -> Self {
        Self {
            portal,
            capture,
            state: Mutex::new(CaptureState::Fresh),
        }
    }
}

struct ActiveCapture {
    // Rust drops fields in declaration order: stop PipeWire before closing the session.
    capture: Box<dyn CaptureSession>,
    session: Arc<PortalSessionLease>,
    stream: PortalStream,
    input: Option<Arc<ReisInputBackend>>,
    input_frame_generation: Option<u64>,
}

enum CaptureState {
    Fresh,
    Active(ActiveCapture),
    Exhausted,
}

impl CaptureState {
    fn active(&self) -> Option<&ActiveCapture> {
        if let Self::Active(active) = self {
            Some(active)
        } else {
            None
        }
    }

    fn active_mut(&mut self) -> Option<&mut ActiveCapture> {
        if let Self::Active(active) = self {
            Some(active)
        } else {
            None
        }
    }
}

impl<P, C> ScreenshotProvider for ScreenshotCoordinator<P, C>
where
    P: PortalBackend,
    C: CaptureBackend,
{
    async fn prepare(&self) -> Result<PrepareCapture, ScreenshotError> {
        let mut state = self.state.lock().await;
        let unavailable = match &*state {
            CaptureState::Active(active) => {
                let failure = active.capture.failure();
                if !active.session.is_closed() && failure.is_none() {
                    return Ok(PrepareCapture {
                        consent_interrupted_observation: false,
                    });
                }
                Some(failure.unwrap_or_else(|| "portal session closed".into()))
            }
            CaptureState::Exhausted => return Err(session_unavailable()),
            CaptureState::Fresh => None,
        };
        if let Some(reason) = unavailable {
            eprintln!("computer-use-mcp: desktop session became unavailable: {reason}");
            exhaust_capture(&mut state, "desktop session became unavailable").await;
            return Err(session_unavailable());
        }
        let connection = self.portal.establish().await.map_err(ScreenshotError)?;
        let session = Arc::clone(&connection.session);
        let consent_interrupted_observation = connection.consent_interrupted_observation;
        let active = async {
            let mut capture = self
                .capture
                .start(connection.fd, connection.stream.capture_target())
                .map_err(ScreenshotError)?;
            tokio::time::timeout(Duration::from_secs(5), capture.wait_ready())
                .await
                .map_err(|_| {
                    ScreenshotError(
                        "timed out waiting for PipeWire shared-memory capture; the source may be DMA-BUF-only"
                            .into(),
                    )
                })?
                .map_err(ScreenshotError)?;
            if connection.session.is_closed() {
                return Err(ScreenshotError(
                    "portal RemoteDesktop session closed during PipeWire startup".into(),
                ));
            }
            Ok(ActiveCapture {
                capture,
                session: connection.session,
                stream: connection.stream,
                input: None,
                input_frame_generation: None,
            })
        }
        .await;
        let active = match active {
            Ok(active) => active,
            Err(error) => {
                if !close_startup_session(&session, "desktop session startup failed").await {
                    *state = CaptureState::Exhausted;
                }
                return Err(error);
            }
        };
        *state = CaptureState::Active(active);
        Ok(PrepareCapture {
            consent_interrupted_observation,
        })
    }

    async fn capture<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> Result<ScreenshotObservation, ScreenshotError> {
        let mut state = self.state.lock().await;
        let active = state
            .active_mut()
            .ok_or_else(|| ScreenshotError("portal capture session is not established".into()))?;
        if active.session.is_closed() {
            eprintln!(
                "computer-use-mcp: capture requested after portal Session.Closed: session={} generation={}",
                active.session.identity(),
                active.session.generation()
            );
            return Err(ScreenshotError(
                "portal RemoteDesktop session closed".into(),
            ));
        }
        let baseline = active
            .capture
            .latest_after(None, Duration::from_secs(2))
            .await
            .map_err(ScreenshotError)?;
        let frame = active
            .capture
            .latest_after(Some(baseline.metadata.generation), Duration::from_secs(2))
            .await
            .map_err(ScreenshotError)?;
        let source = frame.metadata;
        let (width, height) = if source.transform.swaps_axes() {
            (source.crop.height, source.crop.width)
        } else {
            (source.crop.width, source.crop.height)
        };
        let encoded = encoder::encode(
            frame.rgba,
            source.size,
            source.crop,
            source.transform,
            PixelRect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .map_err(ScreenshotError)?;
        if active.session.is_closed() {
            return Err(ScreenshotError(
                "portal session closed while encoding the screenshot".into(),
            ));
        }
        Ok(ScreenshotObservation {
            png_base64: STANDARD.encode(&encoded.bytes),
            mapping: ScreenshotMapping {
                app_pid: snapshot.app.pid,
                app_identity: snapshot.app.object.clone(),
                window_identity: snapshot.window.object.clone(),
                accessibility_generation: snapshot.generation,
                portal_session_identity: active.session.identity().to_owned(),
                portal_session_generation: active.session.generation(),
                stream: active.stream.clone(),
                source,
                output_size: encoded.size,
            },
        })
    }

    async fn prepare_input<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        mapping: &'a ScreenshotMapping,
        action: &'a GeneratedInputAction,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let active = state
            .active_mut()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
        if active.session.is_closed() {
            exhaust_capture(&mut state, "portal session closed before input preparation").await;
            return Err(SESSION_UNAVAILABLE.into());
        }
        ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
        validate_current_capture(active, mapping).await?;
        let keyboard_required = matches!(action, GeneratedInputAction::Keyboard { .. });
        let connected_now = active.input.is_none();
        if connected_now {
            match ReisInputBackend::connect(Arc::clone(&active.session), &mapping.stream).await {
                Ok(input) => active.input = Some(input),
                Err(error) => {
                    exhaust_capture(&mut state, "EIS setup failed").await;
                    return Err(format!("{SESSION_UNAVAILABLE}: EIS setup failed: {error}"));
                }
            }
        }
        let active = state
            .active_mut()
            .ok_or_else(|| "capture state disappeared after EIS setup".to_owned())?;
        if connected_now {
            validate_current_capture(active, mapping).await?;
        }
        let input = active
            .input
            .as_ref()
            .ok_or_else(|| "EIS backend disappeared after setup".to_owned())?;
        let region = input.wait_for_action(keyboard_required).await?;
        ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?
            .eis_mapper(region)?;
        require_action_capabilities(input.as_ref(), action)?;
        if let GeneratedInputAction::Keyboard { action, .. } = action {
            keyboard_input::preflight(input, action)?;
        }
        Ok(())
    }

    async fn perform_input<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        mapping: &'a ScreenshotMapping,
        action: GeneratedInputAction,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let active = state
            .active_mut()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
        if active.session.is_closed() {
            exhaust_capture(&mut state, "portal session closed before generated input").await;
            return Err(SESSION_UNAVAILABLE.into());
        }
        let semantic_keyboard = matches!(
            &action,
            GeneratedInputAction::Keyboard {
                focus: KeyboardFocus::Element(_),
                ..
            }
        );
        if !semantic_keyboard {
            ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
            if let Err(error) = validate_current_capture(active, mapping).await {
                eprintln!("computer-use-mcp: invalidating capture before input: {error}");
                exhaust_capture(&mut state, "capture validation failed before input").await;
                return Err(error);
            }
        }
        let active = state
            .active_mut()
            .ok_or_else(|| "capture state disappeared after pre-input validation".to_owned())?;
        let input = active
            .input
            .as_ref()
            .ok_or_else(|| "EIS input was not prepared for this action".to_owned())?
            .clone();
        let backend: Arc<dyn InputBackend> = input.clone();
        let validated = ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
        let region = input.region()?;
        let mapper = validated.eis_mapper(region)?;
        require_action_capabilities(input.as_ref(), &action)?;

        match action {
            GeneratedInputAction::Pointer(action) => match action {
                PointerAction::Move { x, y } => {
                    let (x, y) = mapper.point(x, y)?;
                    pointer::move_pointer(backend, x, y).await?;
                }
                PointerAction::Click {
                    x,
                    y,
                    button,
                    count,
                } => {
                    let (x, y) = mapper.point(x, y)?;
                    pointer::click(backend, x, y, button, count).await?;
                }
                PointerAction::Drag { from, to } => {
                    let from = mapper.point(from.0, from.1)?;
                    let to = mapper.point(to.0, to.1)?;
                    pointer::drag(backend, from, to).await?;
                }
                PointerAction::Scroll {
                    x,
                    y,
                    delta_x,
                    delta_y,
                } => {
                    let (x, y) = mapper.point(x, y)?;
                    pointer::scroll(backend, x, y, delta_x, delta_y).await?;
                }
            },
            GeneratedInputAction::Keyboard { focus, action } => {
                let focus = match focus {
                    KeyboardFocus::Point((x, y)) => Some(mapper.point(x, y)?),
                    KeyboardFocus::Element(_) => None,
                };
                keyboard_input::perform(input, focus, action).await?;
            }
        }
        if active.session.is_closed() {
            return Err("portal Session.Closed during generated input".into());
        }
        Ok(())
    }

    async fn cleanup_input(&self) -> Result<(), String> {
        let input = {
            let state = self.state.lock().await;
            let Some(active) = state.active() else {
                return Ok(());
            };
            active.input.clone()
        };
        match input {
            Some(input) => input.cleanup_barrier().await,
            None => Ok(()),
        }
    }

    async fn shutdown_input(&self) -> Result<(), String> {
        let active = take_active(&mut *self.state.lock().await);
        let Some(active) = active else {
            return Ok(());
        };
        close_active(active, "computer-use shutdown").await
    }
}

fn take_active(state: &mut CaptureState) -> Option<ActiveCapture> {
    match std::mem::replace(state, CaptureState::Exhausted) {
        CaptureState::Active(active) => Some(active),
        CaptureState::Fresh | CaptureState::Exhausted => None,
    }
}

async fn exhaust_capture(state: &mut CaptureState, reason: &str) {
    if let Some(active) = take_active(state)
        && let Err(error) = close_active(active, reason).await
    {
        eprintln!("computer-use-mcp: exhausted session cleanup failed: {error}");
    }
}

async fn close_active(active: ActiveCapture, reason: &str) -> Result<(), String> {
    let ActiveCapture {
        capture,
        session,
        stream: _,
        input,
        input_frame_generation: _,
    } = active;
    let cleanup = match input.as_ref() {
        Some(input) => tokio::time::timeout(Duration::from_secs(2), input.cleanup_barrier())
            .await
            .unwrap_or_else(|_| Err("timed out neutralizing EIS input during shutdown".to_owned())),
        None => Ok(()),
    };
    drop(capture);
    drop(input);
    let close = tokio::time::timeout(Duration::from_secs(2), session.close(reason))
        .await
        .unwrap_or_else(|_| Err("timed out closing the portal session during shutdown".to_owned()));
    cleanup.and(close)
}

fn session_unavailable() -> ScreenshotError {
    ScreenshotError(SESSION_UNAVAILABLE.into())
}

async fn close_startup_session(session: &PortalSessionLease, reason: &str) -> bool {
    match tokio::time::timeout(Duration::from_secs(2), session.close(reason)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            eprintln!("computer-use-mcp: failed to close partial startup session: {error}");
            false
        }
        Err(_) => {
            eprintln!("computer-use-mcp: timed out closing partial startup session");
            false
        }
    }
}

async fn validate_current_capture(
    active: &mut ActiveCapture,
    mapping: &ScreenshotMapping,
) -> Result<(), String> {
    let after_generation = active
        .input_frame_generation
        .unwrap_or(mapping.source.generation)
        .max(mapping.source.generation);
    let frame = active
        .capture
        .latest_after(Some(after_generation), Duration::from_millis(250))
        .await?;
    verify_current_frame_metadata(&frame, mapping)?;
    active.input_frame_generation = Some(frame.metadata.generation);
    Ok(())
}

fn verify_current_frame_metadata(
    frame: &OwnedFrame,
    mapping: &ScreenshotMapping,
) -> Result<(), String> {
    if frame.metadata.format_generation != mapping.source.format_generation
        || frame.metadata.size != mapping.source.size
        || frame.metadata.crop != mapping.source.crop
        || frame.metadata.transform != mapping.source.transform
    {
        return Err(format!(
            "PipeWire stream metadata renegotiated after screenshot: format_generation={} size={:?} crop={:?} transform={:?}",
            frame.metadata.format_generation,
            frame.metadata.size,
            frame.metadata.crop,
            frame.metadata.transform
        ));
    }
    Ok(())
}

fn require_action_capabilities(
    backend: &ReisInputBackend,
    action: &GeneratedInputAction,
) -> Result<(), String> {
    let (button, scroll, keyboard) = match action {
        GeneratedInputAction::Pointer(PointerAction::Move { .. }) => (false, false, false),
        GeneratedInputAction::Pointer(PointerAction::Click { .. } | PointerAction::Drag { .. }) => {
            (true, false, false)
        }
        GeneratedInputAction::Pointer(PointerAction::Scroll { .. }) => (false, true, false),
        GeneratedInputAction::Keyboard { focus, .. } => {
            (matches!(focus, KeyboardFocus::Point(_)), false, true)
        }
    };
    backend.require_capabilities(button, scroll, keyboard)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        os::fd::OwnedFd,
        os::unix::net::UnixStream,
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::{
        accessibility::{AppInfo, SnapshotLimits, WindowInfo},
        capture::{CaptureFuture, CaptureSession, CaptureTarget},
        geometry::{PixelRect, Transform},
        portal::{PortalCapabilities, PortalConnection},
    };

    struct FakePortal {
        connections: StdMutex<VecDeque<PortalConnection>>,
        establishes: AtomicUsize,
    }

    impl PortalBackend for FakePortal {
        async fn establish(&self) -> Result<PortalConnection, String> {
            self.establishes.fetch_add(1, Ordering::AcqRel);
            self.connections
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "no fake portal connection remains".into())
        }

        async fn capabilities(&self) -> Result<PortalCapabilities, String> {
            Ok(test_capabilities())
        }
    }

    #[derive(Default)]
    struct FakeCaptureState {
        markers: StdMutex<Vec<u8>>,
        failures: StdMutex<Vec<Arc<StdMutex<Option<String>>>>>,
        drops: AtomicUsize,
    }

    struct FakeCaptureBackend(Arc<FakeCaptureState>);

    impl CaptureBackend for FakeCaptureBackend {
        fn start(
            &self,
            fd: OwnedFd,
            target: CaptureTarget,
        ) -> Result<Box<dyn CaptureSession>, String> {
            if target.stream_index != 0 {
                return Err("fake capture received wrong target".into());
            }
            let mut file = std::fs::File::from(fd);
            let mut marker = [0_u8; 1];
            file.read_exact(&mut marker)
                .map_err(|error| format!("restricted fd was not handed to capture: {error}"))?;
            self.0.markers.lock().unwrap().push(marker[0]);
            let failure = Arc::new(StdMutex::new(None));
            self.0.failures.lock().unwrap().push(Arc::clone(&failure));
            Ok(Box::new(FakeCaptureSession {
                failure,
                drops: Arc::clone(&self.0),
            }))
        }
    }

    struct FakeCaptureSession {
        failure: Arc<StdMutex<Option<String>>>,
        drops: Arc<FakeCaptureState>,
    }

    impl CaptureSession for FakeCaptureSession {
        fn wait_ready(&mut self) -> CaptureFuture<'_, ()> {
            let result = self.failure.lock().unwrap().clone().map_or(Ok(()), Err);
            Box::pin(async move { result })
        }

        fn failure(&self) -> Option<String> {
            self.failure.lock().unwrap().clone()
        }

        fn latest_after(
            &mut self,
            after_generation: Option<u64>,
            _wait: Duration,
        ) -> CaptureFuture<'_, OwnedFrame> {
            let failure = self.failure();
            Box::pin(async move {
                if let Some(error) = failure {
                    return Err(error);
                }
                let generation = after_generation.unwrap_or(0) + 1;
                Ok(OwnedFrame {
                    metadata: FrameMetadata {
                        generation,
                        format_generation: 1,
                        size: (2, 2),
                        crop: PixelRect {
                            x: 0,
                            y: 0,
                            width: 2,
                            height: 2,
                        },
                        transform: Transform::Normal,
                    },
                    rgba: vec![255; 16],
                })
            })
        }
    }

    impl Drop for FakeCaptureSession {
        fn drop(&mut self) {
            self.drops.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn test_capabilities() -> PortalCapabilities {
        PortalCapabilities {
            remote_desktop_version: 2,
            screencast_version: 6,
            available_device_types: 7,
            available_source_types: 3,
            available_cursor_modes: 7,
        }
    }

    fn test_connection(
        generation: u64,
        marker: u8,
    ) -> (PortalConnection, tokio::sync::watch::Sender<bool>) {
        let (read, mut write) = UnixStream::pair().unwrap();
        write.write_all(&[marker]).unwrap();
        let (session, closed) = PortalSessionLease::for_test("/session/test", generation);
        (
            PortalConnection {
                fd: read.into(),
                session,
                stream: PortalStream {
                    stream_index: 0,
                    node_id: 10,
                    pipewire_serial: Some(20),
                    id: Some("stream".into()),
                    mapping_id: Some("mapping".into()),
                    position: Some((0, 0)),
                    logical_size: Some((2, 2)),
                },
                consent_interrupted_observation: false,
            },
            closed,
        )
    }

    fn test_coordinator(
        connections: impl IntoIterator<Item = PortalConnection>,
        capture: Arc<FakeCaptureState>,
    ) -> ScreenshotCoordinator<FakePortal, FakeCaptureBackend> {
        ScreenshotCoordinator::new(
            FakePortal {
                connections: StdMutex::new(connections.into_iter().collect()),
                establishes: AtomicUsize::new(0),
            },
            FakeCaptureBackend(capture),
        )
    }

    async fn assert_terminal(coordinator: &ScreenshotCoordinator<FakePortal, FakeCaptureBackend>) {
        for _ in 0..2 {
            assert!(
                coordinator
                    .prepare()
                    .await
                    .unwrap_err()
                    .0
                    .contains("disable and re-enable the MCP")
            );
        }
    }

    fn test_snapshot() -> Snapshot {
        let object = ObjectId {
            bus_name: ":1.5".into(),
            path: "/window".into(),
        };
        Snapshot {
            app_query: "test".into(),
            view: crate::validation::ObservationView::Full,
            element_query: None,
            app: AppInfo {
                object: ObjectId {
                    bus_name: ":1.5".into(),
                    path: "/app".into(),
                },
                name: "Test".into(),
                pid: 5,
                windows: Vec::new(),
            },
            window: WindowInfo {
                object,
                title: "Window".into(),
                states: ["active".into()].into_iter().collect(),
            },
            generation: 1,
            elements: Vec::new(),
            node_limit_reached: false,
            depth_limit_reached: false,
            limits: SnapshotLimits {
                text: 10,
                nodes: 10,
                depth: 10,
            },
        }
    }

    #[tokio::test]
    async fn failed_or_closed_session_requires_mcp_restart_without_another_prompt() {
        let (first, first_closed) = test_connection(1, 11);
        let (second, _) = test_connection(2, 22);
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([first, second], Arc::clone(&capture_state));

        coordinator.prepare().await.unwrap();
        assert_eq!(*capture_state.markers.lock().unwrap(), [11]);
        *capture_state.failures.lock().unwrap()[0].lock().unwrap() =
            Some("target node disappeared".into());
        assert_terminal(&coordinator).await;
        assert_eq!(*capture_state.markers.lock().unwrap(), [11]);
        assert_eq!(capture_state.drops.load(Ordering::Acquire), 1);
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
        drop(first_closed);

        let (connection, closed) = test_connection(3, 33);
        let coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        coordinator.prepare().await.unwrap();
        closed.send_replace(true);
        assert_terminal(&coordinator).await;
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn startup_failure_can_retry_but_shutdown_is_terminal() {
        let (mut broken, _) = test_connection(1, 11);
        let broken_session = Arc::clone(&broken.session);
        broken.stream.stream_index = 1;
        let (valid, _) = test_connection(2, 22);
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([broken, valid], capture_state);

        assert!(coordinator.prepare().await.is_err());
        assert!(broken_session.is_closed());
        coordinator.prepare().await.unwrap();
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 2);
        coordinator.shutdown_input().await.unwrap();
        assert_terminal(&coordinator).await;
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn coordinator_binds_frame_and_session_identity_without_portal_logical_size() {
        let (mut connection, _) = test_connection(9, 44);
        connection.stream.logical_size = None;
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([connection], capture_state);
        coordinator.prepare().await.unwrap();
        let observation = coordinator.capture(&test_snapshot()).await.unwrap();
        assert_eq!(observation.mapping.source.generation, 2);
        assert_eq!(observation.mapping.portal_session_generation, 9);
        assert_eq!(observation.mapping.output_size, (2, 2));
        let matching = OwnedFrame {
            metadata: FrameMetadata {
                generation: observation.mapping.source.generation + 1,
                ..observation.mapping.source
            },
            rgba: vec![0; 16],
        };
        assert!(verify_current_frame_metadata(&matching, &observation.mapping).is_ok());
        let mut same_geometry_new_format = matching.clone();
        same_geometry_new_format.metadata.format_generation += 1;
        assert!(
            verify_current_frame_metadata(&same_geometry_new_format, &observation.mapping)
                .unwrap_err()
                .contains("renegotiated")
        );
        let mut renegotiated = matching;
        renegotiated.metadata.size.0 = 3;
        assert!(
            verify_current_frame_metadata(&renegotiated, &observation.mapping)
                .unwrap_err()
                .contains("renegotiated")
        );
    }

    #[tokio::test]
    async fn capture_succeeds_without_portal_global_position() {
        let (mut connection, _) = test_connection(9, 44);
        connection.stream.position = None;
        let coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        coordinator.prepare().await.unwrap();
        let observation = coordinator.capture(&test_snapshot()).await.unwrap();
        assert_eq!(observation.mapping.stream.position, None);
    }

    #[tokio::test]
    async fn generated_input_without_mapping_id_requires_exact_geometry() {
        let (mut connection, _) = test_connection(9, 44);
        connection.stream.mapping_id = None;
        connection.stream.position = None;
        let coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        coordinator.prepare().await.unwrap();
        let snapshot = test_snapshot();
        let observation = coordinator.capture(&snapshot).await.unwrap();
        let error = coordinator
            .prepare_input(
                &snapshot,
                &observation.mapping,
                &GeneratedInputAction::Pointer(PointerAction::Move { x: 0.0, y: 0.0 }),
            )
            .await
            .unwrap_err();
        assert!(error.contains("omitted mapping_id and position"));
    }
}
