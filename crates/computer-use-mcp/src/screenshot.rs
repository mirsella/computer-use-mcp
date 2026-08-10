use std::{future::Future, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::sync::Mutex;

use crate::{
    accessibility::{ObjectId, Snapshot},
    capture::{CaptureBackend, CaptureSession, FrameMetadata, OwnedFrame, PipeWireCapture},
    encoder,
    input::{
        GeneratedInputAction,
        backend::{ActionProgress, InputBackend, InputError},
        coordinates::ValidatedMapping,
        eis::ReisInputBackend,
        keyboard_input, pointer,
    },
    portal::{PortalBackend, PortalSessionLease, PortalStream, XdgPortalBackend},
    runtime::PostStatus,
    validation::{KeyboardPoint, PointerAction},
};

pub(crate) const SESSION_UNAVAILABLE: &str =
    "desktop session is unavailable; disable and re-enable the MCP to request KDE approval again";
const FRAME_WAIT_BOUND: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotError(pub String);

impl std::fmt::Display for ScreenshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScreenshotError {}

impl ScreenshotError {
    pub fn post_status(&self) -> PostStatus {
        if self.0 == SESSION_UNAVAILABLE {
            PostStatus::SessionUnavailable
        } else if self.0.to_ascii_lowercase().contains("timed out") {
            PostStatus::Timeout
        } else if self.0.to_ascii_lowercase().contains("stream")
            || self.0.to_ascii_lowercase().contains("portal")
        {
            PostStatus::StreamDegraded
        } else {
            PostStatus::CaptureFailed
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameWaitCondition {
    Advanced {
        after_generation: u64,
    },
    Changed {
        after_generation: u64,
        after_change_epoch: u64,
    },
    Stable {
        after_generation: u64,
        after_change_epoch: u64,
        after_format_generation: u64,
        for_duration: Duration,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameWaitEvidence {
    #[cfg(test)]
    pub metadata: FrameMetadata,
    pub mapping: ScreenshotMapping,
    pub changed: Option<bool>,
    pub stable_for_ms: Option<u64>,
}

pub trait ScreenshotProvider: Send + Sync + 'static {
    fn prepare(&self) -> impl Future<Output = Result<(), ScreenshotError>> + Send + '_;
    fn capture<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> impl Future<Output = Result<ScreenshotObservation, ScreenshotError>> + Send + 'a;
    fn wait_for_frame<'a>(
        &'a self,
        _condition: FrameWaitCondition,
        _mapping: &'a ScreenshotMapping,
    ) -> impl Future<Output = Result<FrameWaitEvidence, ScreenshotError>> + Send + 'a {
        async {
            Err(ScreenshotError(
                "frame wait is unavailable for this screenshot backend".into(),
            ))
        }
    }
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
        progress: Arc<ActionProgress>,
    ) -> impl Future<Output = Result<(), InputError>> + Send + 'a;
    fn cleanup_input(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        async { Ok(()) }
    }
    fn shutdown_input(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        self.cleanup_input()
    }
}

fn update_stability(
    metadata: &FrameMetadata,
    baseline_change_epoch: &mut u64,
    baseline_format_generation: &mut u64,
    last_hash: &mut Option<u64>,
    stable_since: &mut Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    for_duration: Duration,
) -> bool {
    if metadata.change_epoch != *baseline_change_epoch
        || metadata.format_generation != *baseline_format_generation
    {
        // A transient change or format renegotiation resets the interval even
        // if the next pixels happen to hash back to the old content.
        *baseline_change_epoch = metadata.change_epoch;
        *baseline_format_generation = metadata.format_generation;
        *last_hash = Some(metadata.content_hash);
        *stable_since = Some(now);
    } else if *last_hash == Some(metadata.content_hash) {
        stable_since.get_or_insert(now);
    } else {
        *last_hash = Some(metadata.content_hash);
        *stable_since = Some(now);
    }
    stable_since
        .is_some_and(|since| for_duration.is_zero() || now.duration_since(since) >= for_duration)
}

#[derive(Debug, Default)]
pub struct NoScreenshots;

impl ScreenshotProvider for NoScreenshots {
    async fn prepare(&self) -> Result<(), ScreenshotError> {
        Ok(())
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
        _progress: Arc<ActionProgress>,
    ) -> Result<(), InputError> {
        Err(InputError::SessionUnavailable(
            "generated input requires a live screenshot provider".into(),
        ))
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
    health_failure: Option<String>,
}

fn terminal_failure(active: &ActiveCapture) -> Option<String> {
    active
        .session
        .is_closed()
        .then(|| "portal session closed".to_owned())
        .or_else(|| active.capture.failure())
        .or_else(|| active.health_failure.clone())
}

async fn latest_frame_checked(
    state: &mut CaptureState,
    after_generation: Option<u64>,
    wait: Duration,
) -> Result<OwnedFrame, ScreenshotError> {
    if state.active().is_none() {
        return Err(session_unavailable());
    }
    if state
        .active()
        .is_some_and(|active| active.session.is_closed())
    {
        exhaust_capture(state, "portal session closed during frame wait").await;
        return Err(session_unavailable());
    }
    let result = {
        let active = state.active_mut().ok_or_else(session_unavailable)?;
        active.capture.latest_after(after_generation, wait).await
    };
    if let Some(reason) = state.active().and_then(terminal_failure) {
        eprintln!(
            "computer-use-mcp: desktop session became unavailable during frame wait: {reason}"
        );
        exhaust_capture(
            state,
            "desktop session became unavailable during frame wait",
        )
        .await;
        return Err(session_unavailable());
    }
    let frame = result.map_err(ScreenshotError)?;
    if frame.metadata.stream_health == crate::capture::StreamHealth::Failed {
        eprintln!(
            "computer-use-mcp: capture returned a terminal failed-health frame during frame wait"
        );
        exhaust_capture(state, "capture stream health failed during frame wait").await;
        return Err(session_unavailable());
    }
    Ok(frame)
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
    async fn prepare(&self) -> Result<(), ScreenshotError> {
        let mut state = self.state.lock().await;
        let unavailable = match &*state {
            CaptureState::Active(active) => match terminal_failure(active) {
                Some(reason) => Some(reason),
                None => {
                    return Ok(());
                }
            },
            CaptureState::Exhausted => return Err(session_unavailable()),
            CaptureState::Fresh => None,
        };
        if let Some(reason) = unavailable {
            eprintln!("computer-use-mcp: desktop session became unavailable: {reason}");
            exhaust_capture(&mut state, "desktop session became unavailable").await;
            return Err(session_unavailable());
        }
        // Cancellation or any startup failure is terminal and must not open another chooser.
        *state = CaptureState::Exhausted;
        let connection = self.portal.establish().await.map_err(ScreenshotError)?;
        let session = Arc::clone(&connection.session);
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
                health_failure: None,
            })
        }
        .await;
        let active = match active {
            Ok(active) => active,
            Err(error) => {
                close_startup_session(&session, "desktop session startup failed").await;
                return Err(error);
            }
        };
        *state = CaptureState::Active(active);
        Ok(())
    }

    async fn capture<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> Result<ScreenshotObservation, ScreenshotError> {
        let mut state = self.state.lock().await;
        let result: Result<ScreenshotObservation, ScreenshotError> = async {
            let active = state.active_mut().ok_or_else(session_unavailable)?;
            if active.session.is_closed() {
                return Err(session_unavailable());
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
            let encoded =
                encoder::encode_frame(frame.rgba, source.size, source.crop, source.transform)
                    .map_err(ScreenshotError)?;
            if active.session.is_closed() {
                return Err(session_unavailable());
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
        .await;
        let terminal = state.active().and_then(terminal_failure);
        if let Some(reason) = terminal {
            eprintln!("computer-use-mcp: desktop session became unavailable: {reason}");
            exhaust_capture(
                &mut state,
                "desktop session became unavailable during capture",
            )
            .await;
            return Err(session_unavailable());
        }
        result
    }

    async fn wait_for_frame(
        &self,
        condition: FrameWaitCondition,
        mapping: &ScreenshotMapping,
    ) -> Result<FrameWaitEvidence, ScreenshotError> {
        let mut state = self.state.lock().await;
        let Some(active) = state.active() else {
            return Err(session_unavailable());
        };
        if active.session.is_closed()
            || active.session.identity() != mapping.portal_session_identity
            || active.session.generation() != mapping.portal_session_generation
            || active.stream != mapping.stream
        {
            eprintln!(
                "computer-use-mcp: refusing frame wait with a stale visual binding: session or stream changed"
            );
            return Err(session_unavailable());
        }
        if mapping.source.stream_health == crate::capture::StreamHealth::Failed {
            return Err(session_unavailable());
        }
        let mut stable_elapsed_ms = None;
        let frame = match condition {
            FrameWaitCondition::Advanced { after_generation } => {
                latest_frame_checked(&mut state, Some(after_generation), FRAME_WAIT_BOUND).await?
            }
            FrameWaitCondition::Changed {
                after_generation,
                after_change_epoch,
            } => {
                let deadline = tokio::time::Instant::now() + FRAME_WAIT_BOUND;
                let mut generation = after_generation;
                loop {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(ScreenshotError(
                            "timed out waiting for a changed frame".into(),
                        ));
                    }
                    let frame =
                        latest_frame_checked(&mut state, Some(generation), remaining).await?;
                    generation = frame.metadata.generation;
                    if frame.metadata.change_epoch > after_change_epoch {
                        break frame;
                    }
                }
            }
            FrameWaitCondition::Stable {
                after_generation,
                after_change_epoch,
                after_format_generation,
                for_duration,
            } => {
                let deadline = tokio::time::Instant::now() + FRAME_WAIT_BOUND;
                let mut generation = after_generation;
                let mut change_epoch = after_change_epoch;
                let mut format_generation = after_format_generation;
                let mut last_hash = None;
                let mut stable_since = None;
                loop {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(ScreenshotError(
                            "timed out waiting for a stable frame with fresh evidence".into(),
                        ));
                    }
                    let frame =
                        latest_frame_checked(&mut state, Some(generation), remaining).await?;
                    generation = frame.metadata.generation;
                    let now = tokio::time::Instant::now();
                    if update_stability(
                        &frame.metadata,
                        &mut change_epoch,
                        &mut format_generation,
                        &mut last_hash,
                        &mut stable_since,
                        now,
                        for_duration,
                    ) {
                        stable_elapsed_ms = stable_since.map(|since| {
                            now.duration_since(since)
                                .as_millis()
                                .min(u128::from(u64::MAX)) as u64
                        });
                        break frame;
                    }
                }
            }
        };
        let stable_for_ms = match condition {
            FrameWaitCondition::Stable { .. } => stable_elapsed_ms,
            _ => None,
        };
        let mut bound_mapping = mapping.clone();
        let encoded = encoder::encode_frame(
            frame.rgba,
            frame.metadata.size,
            frame.metadata.crop,
            frame.metadata.transform,
        )
        .map_err(ScreenshotError)?;
        bound_mapping.source = frame.metadata;
        bound_mapping.output_size = encoded.size;
        Ok(FrameWaitEvidence {
            changed: match condition {
                FrameWaitCondition::Changed { .. } => Some(true),
                FrameWaitCondition::Stable {
                    after_change_epoch,
                    after_format_generation,
                    ..
                } => Some(
                    frame.metadata.change_epoch != after_change_epoch
                        || frame.metadata.format_generation != after_format_generation,
                ),
                FrameWaitCondition::Advanced { .. } => frame.metadata.changed_from_previous,
            },
            #[cfg(test)]
            metadata: frame.metadata,
            mapping: bound_mapping,
            stable_for_ms,
        })
    }

    async fn prepare_input<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        mapping: &'a ScreenshotMapping,
        action: &'a GeneratedInputAction,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if state
            .active()
            .is_none_or(|active| active.session.is_closed())
        {
            exhaust_capture(&mut state, "portal session closed before input preparation").await;
            return Err(SESSION_UNAVAILABLE.into());
        }
        {
            let active = state
                .active()
                .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
            ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
        }
        validate_current_capture_state(&mut state, mapping, "before input preparation").await?;
        let keyboard_required = matches!(action, GeneratedInputAction::KeyboardTransaction { .. });
        let connected_now = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .input
            .is_none();
        if connected_now {
            let session = Arc::clone(
                &state
                    .active()
                    .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
                    .session,
            );
            match ReisInputBackend::connect(session, &mapping.stream).await {
                Ok(input) => {
                    state
                        .active_mut()
                        .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
                        .input = Some(input)
                }
                Err(error) => {
                    exhaust_capture(&mut state, "EIS setup failed").await;
                    return Err(format!("{SESSION_UNAVAILABLE}: EIS setup failed: {error}"));
                }
            }
        }
        if connected_now {
            validate_current_capture_state(&mut state, mapping, "after EIS setup").await?;
        }
        let input = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .input
            .as_ref()
            .ok_or_else(|| "EIS backend disappeared after setup".to_owned())?;
        let input = Arc::clone(input);
        let region = match input.wait_for_action(keyboard_required).await {
            Ok(region) => region,
            Err(error) => {
                if state.active().and_then(terminal_failure).is_some() {
                    exhaust_capture(&mut state, "desktop session failed while preparing input")
                        .await;
                    return Err(SESSION_UNAVAILABLE.into());
                }
                return Err(error);
            }
        };
        if let Some(failure) = state.active().and_then(terminal_failure) {
            exhaust_capture(&mut state, "desktop session failed while preparing input").await;
            return Err(format!("{SESSION_UNAVAILABLE}: {failure}"));
        }
        validate_current_capture_state(&mut state, mapping, "after EIS synchronization").await?;
        let active = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
        let mapper = ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?
            .eis_mapper(region)?;
        require_action_capabilities(input.as_ref(), action)?;
        match action {
            GeneratedInputAction::Pointer(action) => preflight_pointer(&mapper, action)?,
            GeneratedInputAction::KeyboardTransaction { focus, events } => {
                let focus = mapper.point(focus.x, focus.y)?;
                keyboard_input::preflight_transaction(
                    &input,
                    KeyboardPoint {
                        x: focus.0,
                        y: focus.1,
                    },
                    events,
                )?;
            }
        }
        Ok(())
    }

    async fn perform_input<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        mapping: &'a ScreenshotMapping,
        action: GeneratedInputAction,
        progress: Arc<ActionProgress>,
    ) -> Result<(), InputError> {
        let mut state = self.state.lock().await;
        if state
            .active()
            .is_none_or(|active| active.session.is_closed())
        {
            exhaust_capture(&mut state, "portal session closed before generated input").await;
            return Err(InputError::SessionUnavailable(SESSION_UNAVAILABLE.into()));
        }
        {
            let active = state
                .active()
                .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
            ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
        }
        validate_current_capture_state(&mut state, mapping, "before input dispatch").await?;
        let input = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .input
            .as_ref()
            .ok_or_else(|| "EIS input was not prepared for this action".to_owned())?
            .clone();
        let backend: Arc<dyn InputBackend> = input.clone();
        let active = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
        let validated = ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
        let region = input.region()?;
        let mapper = validated.eis_mapper(region)?;
        require_action_capabilities(input.as_ref(), &action)?;

        let result: Result<(), String> = async {
            match action {
                GeneratedInputAction::Pointer(action) => match action {
                    PointerAction::Move { x, y } => {
                        let (x, y) = mapper.point(x, y)?;
                        pointer::move_pointer(backend, x, y, Arc::clone(&progress)).await?;
                    }
                    PointerAction::Click {
                        x,
                        y,
                        button,
                        count,
                    } => {
                        let (x, y) = mapper.point(x, y)?;
                        pointer::click(backend, x, y, button, count, Arc::clone(&progress)).await?;
                    }
                    PointerAction::Drag { path } => {
                        let path = path
                            .into_iter()
                            .map(|point| mapper.point(point.0, point.1))
                            .collect::<Result<Vec<_>, _>>()?;
                        pointer::drag_path(backend, path, Arc::clone(&progress)).await?;
                    }
                    PointerAction::Scroll {
                        x,
                        y,
                        delta_x,
                        delta_y,
                    } => {
                        let (x, y) = mapper.point(x, y)?;
                        pointer::scroll(backend, x, y, delta_x, delta_y, Arc::clone(&progress))
                            .await?;
                    }
                },
                GeneratedInputAction::KeyboardTransaction { focus, events } => {
                    let focus = mapper.point(focus.x, focus.y)?;
                    let focus = KeyboardPoint {
                        x: focus.0,
                        y: focus.1,
                    };
                    keyboard_input::perform_transaction(
                        input,
                        focus,
                        events,
                        Arc::clone(&progress),
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .await;
        result.map_err(InputError::from)
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
        health_failure: _,
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

async fn close_startup_session(session: &PortalSessionLease, reason: &str) {
    match tokio::time::timeout(Duration::from_secs(2), session.close(reason)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("computer-use-mcp: failed to close partial startup session: {error}");
        }
        Err(_) => {
            eprintln!("computer-use-mcp: timed out closing partial startup session");
        }
    }
}

fn validate_current_capture(
    active: &mut ActiveCapture,
    mapping: &ScreenshotMapping,
) -> Result<(), String> {
    let metadata = match active.capture.current_metadata() {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            return Err(
                "capture has no currently committed complete frame; refusing generated input"
                    .into(),
            );
        }
        Err(error) => {
            if let Some(reason) = terminal_failure(active) {
                active.health_failure = Some(reason);
            }
            return Err(error);
        }
    };
    if metadata.stream_health == crate::capture::StreamHealth::Failed {
        let reason = "capture stream health is failed".to_owned();
        active.health_failure = Some(reason.clone());
        return Err(reason);
    }
    if metadata.stream_health == crate::capture::StreamHealth::Degraded {
        return Err("capture stream health is degraded; refusing generated input".into());
    }
    verify_current_frame_metadata(&metadata, mapping)?;
    Ok(())
}

async fn validate_current_capture_state(
    state: &mut CaptureState,
    mapping: &ScreenshotMapping,
    reason: &str,
) -> Result<(), String> {
    let result = {
        let active = state
            .active_mut()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?;
        validate_current_capture(active, mapping)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(failure) = state.active().and_then(terminal_failure) {
                eprintln!("computer-use-mcp: desktop session became unavailable: {failure}");
                exhaust_capture(state, reason).await;
                return Err(format!("{SESSION_UNAVAILABLE}: {failure}"));
            }
            eprintln!(
                "computer-use-mcp: generated input frame validation failed {reason}: {error}"
            );
            Err(error)
        }
    }
}

fn verify_current_frame_metadata(
    metadata: &FrameMetadata,
    mapping: &ScreenshotMapping,
) -> Result<(), String> {
    if metadata.format_generation != mapping.source.format_generation
        || metadata.size != mapping.source.size
        || metadata.crop != mapping.source.crop
        || metadata.transform != mapping.source.transform
    {
        return Err(format!(
            "PipeWire stream metadata renegotiated after screenshot: format_generation={} size={:?} crop={:?} transform={:?}",
            metadata.format_generation, metadata.size, metadata.crop, metadata.transform
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
        GeneratedInputAction::KeyboardTransaction { .. } => (false, false, true),
    };
    backend.require_capabilities(button, scroll, keyboard)
}

fn preflight_pointer(
    mapper: &crate::input::coordinates::AbsoluteMapper,
    action: &PointerAction,
) -> Result<(), String> {
    match action {
        PointerAction::Move { x, y }
        | PointerAction::Click { x, y, .. }
        | PointerAction::Scroll { x, y, .. } => {
            mapper.point(*x, *y)?;
        }
        PointerAction::Drag { path } => {
            for point in path {
                mapper.point(point.0, point.1)?;
            }
        }
    }
    Ok(())
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
        never_fresh: AtomicUsize,
        failed_health: AtomicUsize,
        format_generation: AtomicUsize,
        current_metadata_checks: AtomicUsize,
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
                never_fresh: self.0.never_fresh.load(Ordering::Acquire) != 0,
            }))
        }
    }

    struct FakeCaptureSession {
        failure: Arc<StdMutex<Option<String>>>,
        drops: Arc<FakeCaptureState>,
        never_fresh: bool,
    }

    impl CaptureSession for FakeCaptureSession {
        fn wait_ready(&mut self) -> CaptureFuture<'_, ()> {
            let result = self.failure.lock().unwrap().clone().map_or(Ok(()), Err);
            Box::pin(async move { result })
        }

        fn failure(&self) -> Option<String> {
            self.failure.lock().unwrap().clone()
        }

        fn current_metadata(&self) -> Result<Option<FrameMetadata>, String> {
            self.drops
                .current_metadata_checks
                .fetch_add(1, Ordering::AcqRel);
            if let Some(error) = self.failure() {
                return Err(error);
            }
            Ok(Some(fake_metadata(
                1,
                self.drops.format_generation.load(Ordering::Acquire).max(1) as u64,
                self.drops.failed_health.load(Ordering::Acquire) != 0,
            )))
        }

        fn latest_after(
            &mut self,
            after_generation: Option<u64>,
            _wait: Duration,
        ) -> CaptureFuture<'_, OwnedFrame> {
            let failure = self.failure();
            let never_fresh = self.never_fresh;
            let failed_health = self.drops.failed_health.load(Ordering::Acquire) != 0;
            Box::pin(async move {
                if let Some(error) = failure {
                    return Err(error);
                }
                if never_fresh {
                    return Err("timed out waiting for a complete frame".into());
                }
                let generation = after_generation.unwrap_or(0) + 1;
                Ok(OwnedFrame {
                    metadata: fake_metadata(generation, 1, failed_health),
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

    fn fake_metadata(
        generation: u64,
        format_generation: u64,
        failed_health: bool,
    ) -> FrameMetadata {
        FrameMetadata {
            generation,
            format_generation,
            source_sequence: Some(generation),
            pts_ns: Some(i64::try_from(generation).unwrap_or_default()),
            arrival_monotonic_ns: generation,
            size: (2, 2),
            crop: PixelRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            transform: Transform::Normal,
            timestamp_authority: crate::capture::TimestampAuthority::SpaHeader,
            stream_health: if failed_health {
                crate::capture::StreamHealth::Failed
            } else {
                crate::capture::StreamHealth::Healthy
            },
            content_hash: 0,
            change_epoch: 0,
            changed_from_previous: None,
            sequence_gap: None,
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
            view: crate::validation::AccessibilityScope::Full,
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
            element_ids: Vec::new(),
            node_limit_reached: false,
            depth_limit_reached: false,
            limits: SnapshotLimits {
                text: 10,
                nodes: 10,
                depth: 10,
            },
            target_ref: None,
            accessibility_ready: true,
            accessibility_reason: None,
            requires_atspi_revalidation: true,
            screenshot_requested: true,
        }
    }

    fn test_mapping() -> ScreenshotMapping {
        let crop = PixelRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        ScreenshotMapping {
            app_pid: 5,
            app_identity: ObjectId {
                bus_name: ":1.5".into(),
                path: "/app".into(),
            },
            window_identity: ObjectId {
                bus_name: ":1.5".into(),
                path: "/window".into(),
            },
            accessibility_generation: 1,
            portal_session_identity: "/session/test".into(),
            portal_session_generation: 1,
            stream: PortalStream {
                stream_index: 0,
                node_id: 10,
                pipewire_serial: Some(20),
                id: Some("stream".into()),
                mapping_id: Some("mapping".into()),
                position: Some((0, 0)),
                logical_size: Some((2, 2)),
            },
            source: FrameMetadata {
                generation: 0,
                format_generation: 1,
                source_sequence: Some(0),
                pts_ns: Some(0),
                arrival_monotonic_ns: 0,
                size: (2, 2),
                crop,
                transform: Transform::Normal,
                timestamp_authority: crate::capture::TimestampAuthority::SpaHeader,
                stream_health: crate::capture::StreamHealth::Healthy,
                content_hash: 0,
                change_epoch: 0,
                changed_from_previous: None,
                sequence_gap: None,
            },
            output_size: (2, 2),
        }
    }

    #[tokio::test]
    async fn failed_or_closed_session_requires_mcp_restart_without_another_prompt() {
        let (first, _) = test_connection(1, 11);
        let (second, _) = test_connection(2, 22);
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([first, second], Arc::clone(&capture_state));

        coordinator.prepare().await.unwrap();
        assert_eq!(*capture_state.markers.lock().unwrap(), [11]);
        *capture_state.failures.lock().unwrap()[0].lock().unwrap() =
            Some("target node disappeared".into());
        assert!(coordinator.capture(&test_snapshot()).await.is_err());
        assert_terminal(&coordinator).await;
        assert_eq!(*capture_state.markers.lock().unwrap(), [11]);
        assert_eq!(capture_state.drops.load(Ordering::Acquire), 1);
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
        let (connection, closed) = test_connection(3, 33);
        let coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        coordinator.prepare().await.unwrap();
        closed.send_replace(true);
        assert!(coordinator.capture(&test_snapshot()).await.is_err());
        assert_terminal(&coordinator).await;
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn stable_wait_requires_fresh_frame_evidence() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.never_fresh.store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();
        let error = coordinator
            .wait_for_frame(
                FrameWaitCondition::Stable {
                    after_generation: 0,
                    after_change_epoch: 0,
                    after_format_generation: 1,
                    for_duration: Duration::from_millis(50),
                },
                &test_mapping(),
            )
            .await
            .unwrap_err();
        assert!(error.0.contains("timed out"));
    }

    #[tokio::test]
    async fn frame_wait_binds_the_new_frame_to_the_existing_visual_mapping() {
        let (connection, _) = test_connection(1, 11);
        let coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        coordinator.prepare().await.unwrap();
        let evidence = coordinator
            .wait_for_frame(
                FrameWaitCondition::Advanced {
                    after_generation: 0,
                },
                &test_mapping(),
            )
            .await
            .unwrap();
        assert_eq!(evidence.metadata.generation, 1);
        assert_eq!(evidence.mapping.source.generation, 1);
        assert_eq!(evidence.mapping.source.format_generation, 1);
        assert_eq!(evidence.mapping.output_size, (2, 2));
    }

    #[tokio::test]
    async fn terminal_failed_stream_health_exhausts_before_generated_input() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.failed_health.store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();
        let error = coordinator
            .prepare_input(
                &test_snapshot(),
                &test_mapping(),
                &GeneratedInputAction::Pointer(PointerAction::Move { x: 1.0, y: 1.0 }),
            )
            .await
            .unwrap_err();
        assert!(error.contains("disable and re-enable the MCP"));
        assert_terminal(&coordinator).await;
    }

    #[tokio::test]
    async fn repeated_input_validation_reuses_static_source_mapping_without_newer_frame() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();
        let mapping = test_mapping();

        let mut state = coordinator.state.lock().await;
        validate_current_capture_state(&mut state, &mapping, "static source validation")
            .await
            .unwrap();
        validate_current_capture_state(&mut state, &mapping, "repeated static source validation")
            .await
            .unwrap();

        assert_eq!(
            capture_state
                .current_metadata_checks
                .load(Ordering::Acquire),
            2
        );
    }

    #[tokio::test]
    async fn slow_frame_delivery_does_not_block_fresh_source_input_validation() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.never_fresh.store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();

        let mut state = coordinator.state.lock().await;
        tokio::time::timeout(
            Duration::from_millis(25),
            validate_current_capture_state(&mut state, &test_mapping(), "slow frame validation"),
        )
        .await
        .expect("pre-dispatch validation must not wait for a later complete frame")
        .unwrap();
    }

    #[tokio::test]
    async fn input_validation_rejects_current_format_generation_change() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.format_generation.store(2, Ordering::Release);
        let coordinator = test_coordinator([connection], capture_state);
        coordinator.prepare().await.unwrap();

        let mut state = coordinator.state.lock().await;
        let error = validate_current_capture_state(
            &mut state,
            &test_mapping(),
            "format generation validation",
        )
        .await
        .unwrap_err();
        assert!(error.contains("renegotiated"));
    }

    #[test]
    fn stability_resets_on_transient_change_and_format_renegotiation() {
        let mut change_epoch = 0;
        let mut format_generation = 1;
        let mut last_hash = None;
        let mut stable_since = None;
        let start = tokio::time::Instant::now();
        let frame = |generation, format_generation, change_epoch, content_hash| FrameMetadata {
            generation,
            format_generation,
            source_sequence: Some(generation),
            pts_ns: Some(generation as i64),
            arrival_monotonic_ns: generation,
            size: (2, 2),
            crop: PixelRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            transform: Transform::Normal,
            timestamp_authority: crate::capture::TimestampAuthority::SpaHeader,
            stream_health: crate::capture::StreamHealth::Healthy,
            content_hash,
            change_epoch,
            changed_from_previous: None,
            sequence_gap: None,
        };

        assert!(!update_stability(
            &frame(1, 1, 0, 7),
            &mut change_epoch,
            &mut format_generation,
            &mut last_hash,
            &mut stable_since,
            start,
            Duration::from_millis(50),
        ));
        assert!(update_stability(
            &frame(2, 1, 0, 7),
            &mut change_epoch,
            &mut format_generation,
            &mut last_hash,
            &mut stable_since,
            start + Duration::from_millis(60),
            Duration::from_millis(50),
        ));
        assert!(!update_stability(
            &frame(3, 1, 1, 7),
            &mut change_epoch,
            &mut format_generation,
            &mut last_hash,
            &mut stable_since,
            start + Duration::from_millis(61),
            Duration::from_millis(50),
        ));
        assert!(!update_stability(
            &frame(4, 2, 1, 7),
            &mut change_epoch,
            &mut format_generation,
            &mut last_hash,
            &mut stable_since,
            start + Duration::from_millis(62),
            Duration::from_millis(50),
        ));
        assert!(update_stability(
            &frame(5, 2, 1, 7),
            &mut change_epoch,
            &mut format_generation,
            &mut last_hash,
            &mut stable_since,
            start + Duration::from_millis(120),
            Duration::from_millis(50),
        ));
    }

    #[tokio::test]
    async fn startup_failure_is_terminal_without_another_portal_request() {
        let (mut broken, _) = test_connection(1, 11);
        let broken_session = Arc::clone(&broken.session);
        broken.stream.stream_index = 1;
        let (valid, _) = test_connection(2, 22);
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([broken, valid], capture_state);

        assert!(coordinator.prepare().await.is_err());
        assert!(broken_session.is_closed());
        assert_terminal(&coordinator).await;
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);

        let coordinator = test_coordinator([], Arc::new(FakeCaptureState::default()));
        assert!(coordinator.prepare().await.is_err());
        assert_terminal(&coordinator).await;
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
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
        let matching = FrameMetadata {
            generation: observation.mapping.source.generation + 1,
            ..observation.mapping.source
        };
        assert!(verify_current_frame_metadata(&matching, &observation.mapping).is_ok());
        let mut same_geometry_new_format = matching;
        same_geometry_new_format.format_generation += 1;
        assert!(
            verify_current_frame_metadata(&same_geometry_new_format, &observation.mapping)
                .unwrap_err()
                .contains("renegotiated")
        );
        let mut renegotiated = matching;
        renegotiated.size.0 = 3;
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
