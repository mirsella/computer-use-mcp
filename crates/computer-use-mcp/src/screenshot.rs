use std::{future::Future, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::sync::Mutex;

use crate::{
    accessibility::{ObjectId, Snapshot},
    capture::{CaptureBackend, CaptureSession, FrameMetadata, OwnedFrame, PipeWireCapture},
    encoder,
    geometry::window_crop_in_frame,
    input::{
        GeneratedInputAction,
        backend::{ActionProgress, InputBackend, InputError},
        coordinates::{StreamExtent, ValidatedMapping},
        eis::ReisInputBackend,
        keyboard_input, pointer,
    },
    portal::{PortalBackend, PortalSessionLease, PortalStream, XdgPortalBackend},
    runtime::PostStatus,
    validation::{KeyboardEvent, KeyboardFocus, KeyboardPoint, ObserveCrop, PointerAction},
    window_backend::WindowGeometry,
};

pub(crate) const SESSION_UNAVAILABLE: &str = "desktop session is unavailable; request a new approved desktop session and fresh observation; do not retry dispatched input blindly";
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
    /// Requested PNG scope. Monitor is the authoritative full-frame capture;
    /// TargetWindow is an advisory server-side crop with input remapping.
    pub crop: ObserveCrop,
    /// Effective pre-transform source-pixel rect actually encoded when
    /// crop is TargetWindow. Input coordinates remap through this rect back
    /// to monitor space; None for full-monitor captures.
    pub window_crop_source: Option<crate::geometry::PixelRect>,
    /// KDE geometry that justified `window_crop_source`. Waits and input must
    /// never reuse a target crop without this authoritative binding.
    pub window_crop_geometry: Option<WindowGeometry>,
    /// True only when `window_crop_source` was applied before transform and
    /// PNG budgeting. Legacy providers may expose a post-encoded crop for the
    /// initial observation, but waits must invalidate that view and reobserve.
    pub window_crop_is_preencoded: bool,
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
    /// Capture the requested observation view using authoritative KDE window
    /// geometry when a target-window crop was requested. Backends without a
    /// raw-frame crop path retain the old capture behavior; the observation
    /// layer may then apply its explicit compatibility fallback.
    fn capture_with_window_geometry<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        _window_geometry: Option<WindowGeometry>,
    ) -> impl Future<Output = Result<ScreenshotObservation, ScreenshotError>> + Send + 'a {
        async move { self.capture(snapshot).await }
    }
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
    /// Prepare EIS dispatch for the already-focused element: same live
    /// session, keyboard capability, and text-bound checks as
    /// [`ScreenshotProvider::prepare_input`] but with no screenshot mapping
    /// and no coordinate validation, for windows screenshots cannot see.
    fn prepare_focused_input<'a>(
        &'a self,
        _action: &'a GeneratedInputAction,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a {
        async { Ok(()) }
    }
    /// Dispatch EIS keystrokes to the already-focused element without moving
    /// the pointer. Callers must verify AT-SPI focus first.
    fn perform_focused_input<'a>(
        &'a self,
        _action: GeneratedInputAction,
        _progress: Arc<ActionProgress>,
    ) -> impl Future<Output = Result<(), InputError>> + Send + 'a {
        async {
            Err(InputError::SessionUnavailable(
                "focused-element typing requires a live screenshot provider".into(),
            ))
        }
    }
    fn cleanup_input(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        async { Ok(()) }
    }
    /// Union-disambiguation evidence for the last prepared/dispatched pointer
    /// action, if several resumed EIS regions were resolved into one binding.
    /// Defaults to none so test backends are unaffected.
    fn eis_region_evidence(&self) -> Option<String> {
        None
    }
    /// Mechanism for the last successfully dispatched paste. Read after a
    /// successful perform_input/perform_focused_input, before the next action.
    /// A portal transfer confirms bytes were supplied, not application consumption.
    fn paste_delivery_mechanism(&self) -> Option<&'static str> {
        None
    }
    /// Completed lifecycle state for worker retirement, not action retry.
    fn desktop_session_exhausted(&self) -> bool {
        false
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
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Last union-disambiguation evidence (`eis_region=...`), set during
    /// pointer preparation/dispatch and read after dispatch completes.
    eis_evidence: std::sync::Mutex<Option<String>>,
    paste_delivery: std::sync::Mutex<Option<&'static str>>,
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
            shutdown: tokio::sync::watch::channel(false).0,
            eis_evidence: std::sync::Mutex::new(None),
            paste_delivery: std::sync::Mutex::new(None),
        }
    }

    /// Drop any previous disambiguation evidence. A poisoned evidence cell
    /// only loses a diagnostic string (the mapping itself stays fail-closed),
    /// so poisoning is ignored rather than fatal.
    fn clear_eis_evidence(&self) {
        if let Ok(mut guard) = self.eis_evidence.lock() {
            *guard = None;
        }
    }

    fn store_eis_evidence(&self, evidence: Option<String>) {
        if evidence.is_none() {
            return;
        }
        if let Ok(mut guard) = self.eis_evidence.lock() {
            *guard = evidence;
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
    fn desktop_session_exhausted(&self) -> bool {
        // A lock owner is still preparing or using the session. In particular,
        // prepare temporarily stores Exhausted while consent is pending. Only
        // inspect idle state, as the broker does after the original response.
        self.state.try_lock().is_ok_and(|state| match &*state {
            CaptureState::Fresh => false,
            CaptureState::Exhausted => true,
            CaptureState::Active(active) => terminal_failure(active).is_some(),
        })
    }

    async fn prepare(&self) -> Result<(), ScreenshotError> {
        let mut shutdown = self.shutdown.subscribe();
        let mut state = self.state.lock().await;
        if *shutdown.borrow() {
            return Err(ScreenshotError(
                "desktop initialization cancelled by shutdown".into(),
            ));
        }
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
        // One caller owns initialization under this lock. Portal expiration is
        // retried inside establish; cancellation/denial must not let a queued
        // caller silently reopen a chooser for the same initialization request.
        *state = CaptureState::Exhausted;
        let connection = tokio::select! {
            biased;
            _ = shutdown.changed() => return Err(ScreenshotError("desktop initialization cancelled by shutdown".into())),
            connection = self.portal.establish() => connection.map_err(ScreenshotError)?,
        };
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
        self.capture_with_window_geometry(snapshot, None).await
    }

    async fn capture_with_window_geometry<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        window_geometry: Option<WindowGeometry>,
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
            let frame = match active
                .capture
                .latest_after(Some(baseline.metadata.generation), Duration::from_secs(2))
                .await
            {
                Ok(frame) => frame,
                Err(error) if error == "timed out waiting for a complete frame" => {
                    let current = active
                        .capture
                        .current_metadata()
                        .map_err(ScreenshotError)?;
                    let Some(current) = current else {
                        return Err(ScreenshotError(
                            "cannot reuse startup baseline: no current complete frame remains"
                                .into(),
                        ));
                    };
                    if current.generation != baseline.metadata.generation
                        || current.format_generation != baseline.metadata.format_generation
                    {
                        return Err(ScreenshotError(format!(
                            "cannot reuse startup baseline generation={} format_generation={}; latest available frame is generation={} format_generation={}",
                            baseline.metadata.generation,
                            baseline.metadata.format_generation,
                            current.generation,
                            current.format_generation,
                        )));
                    }
                    eprintln!(
                        "computer-use-mcp: reusing latest available capture frame generation={} format_generation={} after no newer startup frame",
                        baseline.metadata.generation, baseline.metadata.format_generation
                    );
                    // A complete baseline is still the latest available
                    // frame. A newly-started portal stream can briefly
                    // deliver only that frame while its producer settles;
                    // explicit frame waits remain strict.
                    baseline
                }
                Err(error) => return Err(ScreenshotError(error)),
            };
            let source = frame.metadata;
            let (encode_crop, window_crop_source, window_crop_geometry, effective_crop) =
                match snapshot.crop {
                    ObserveCrop::Monitor => (source.crop, None, None, ObserveCrop::Monitor),
                    ObserveCrop::TargetWindow => match window_geometry {
                        None => {
                            // Keep the full monitor when KDE did not provide an
                            // authoritative geometry. The observation layer emits
                            // an explicit monitor-fallback report; it must not
                            // invent a crop from AT-SPI extents.
                            (source.crop, None, None, ObserveCrop::Monitor)
                        }
                        Some(geometry) => {
                            let source_rect = window_crop_in_frame(
                                (geometry.x, geometry.y, geometry.width, geometry.height),
                                active.stream.position,
                                active.stream.logical_size,
                                source.size,
                            )
                            .map_err(ScreenshotError)?;
                            if !source_rect.is_valid_within(source.size)
                                || source_rect.x < source.crop.x
                                || source_rect.y < source.crop.y
                                || source_rect.x.checked_add(source_rect.width).is_none_or(
                                    |right| right > source.crop.x.saturating_add(source.crop.width),
                                )
                                || source_rect.y.checked_add(source_rect.height).is_none_or(
                                    |bottom| {
                                        bottom > source.crop.y.saturating_add(source.crop.height)
                                    },
                                )
                            {
                                return Err(ScreenshotError(
                                "authoritative KDE window crop is outside the encoded source crop"
                                    .into(),
                            ));
                            }
                            (
                                source_rect,
                                Some(source_rect),
                                Some(geometry),
                                ObserveCrop::TargetWindow,
                            )
                        }
                    },
                };
            let encoded =
                encoder::encode_frame(frame.rgba, source.size, encode_crop, source.transform)
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
                    crop: effective_crop,
                    window_crop_source,
                    window_crop_geometry,
                    window_crop_is_preencoded: window_crop_source.is_some(),
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
        verify_current_frame_metadata(&frame.metadata, mapping).map_err(ScreenshotError)?;
        let encode_crop = match (mapping.crop, mapping.window_crop_source) {
            (ObserveCrop::Monitor, None) => frame.metadata.crop,
            (ObserveCrop::TargetWindow, Some(source_rect)) => source_rect,
            (ObserveCrop::TargetWindow, None) => {
                return Err(ScreenshotError(
                    "target-window frame wait has no authoritative crop binding; re-observe".into(),
                ));
            }
            (ObserveCrop::Monitor, Some(_)) => {
                return Err(ScreenshotError(
                    "monitor frame wait has an inconsistent target crop binding; re-observe".into(),
                ));
            }
        };
        if mapping.crop == ObserveCrop::TargetWindow && !mapping.window_crop_is_preencoded {
            return Err(ScreenshotError(
                "target-window frame wait cannot extend a legacy post-encoded crop; re-observe"
                    .into(),
            ));
        }
        if !encode_crop.is_valid_within(frame.metadata.size)
            || encode_crop.x < frame.metadata.crop.x
            || encode_crop.y < frame.metadata.crop.y
            || encode_crop
                .x
                .checked_add(encode_crop.width)
                .is_none_or(|right| {
                    right
                        > frame
                            .metadata
                            .crop
                            .x
                            .saturating_add(frame.metadata.crop.width)
                })
            || encode_crop
                .y
                .checked_add(encode_crop.height)
                .is_none_or(|bottom| {
                    bottom
                        > frame
                            .metadata
                            .crop
                            .y
                            .saturating_add(frame.metadata.crop.height)
                })
        {
            return Err(ScreenshotError(
                "frame wait crop binding is outside the current source crop; re-observe".into(),
            ));
        }
        let encoded = encoder::encode_frame(
            frame.rgba,
            frame.metadata.size,
            encode_crop,
            frame.metadata.transform,
        )
        .map_err(ScreenshotError)?;
        if encoded.size != mapping.output_size {
            return Err(ScreenshotError(
                "frame wait changed the encoded view size; re-observe before acting".into(),
            ));
        }
        let mut bound_mapping = mapping.clone();
        bound_mapping.source = frame.metadata;
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
        // A new preparation invalidates any previous disambiguation evidence;
        // the fresh evidence is stored below and read after dispatch.
        self.clear_eis_evidence();
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
        let keyboard_required = matches!(
            action,
            GeneratedInputAction::KeyboardTransaction { .. } | GeneratedInputAction::Paste { .. }
        );
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
        let points = action_png_points(action)?;
        let window_cropped = mapping.window_crop_source.is_some();
        let stream_extent = StreamExtent::from_portal_stream(&mapping.stream);
        let resolved = match input
            .wait_for_resolved_action(
                keyboard_required,
                mapping.output_size,
                stream_extent,
                window_cropped,
                &points,
            )
            .await
        {
            Ok(resolved) => resolved,
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
        let clipboard_available = active.session.clipboard().is_some();
        let mapper = ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?
            .eis_mapper(resolved.region)?;
        if let Some(evidence) = resolved.evidence {
            eprintln!(
                "computer-use-mcp: EIS union disambiguation engaged; several resumed regions resolved to one binding (eis_region={evidence})"
            );
            self.store_eis_evidence(Some(evidence));
        }
        require_action_capabilities(input.as_ref(), action)?;
        match action {
            GeneratedInputAction::Pointer(action) => preflight_pointer(&mapper, action)?,
            GeneratedInputAction::KeyboardTransaction {
                focus: KeyboardFocus::Point(focus),
                events,
            } => {
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
            GeneratedInputAction::Paste {
                focus: KeyboardFocus::Point(focus),
                text,
            } => {
                let focus = mapper.point(focus.x, focus.y)?;
                let focus = KeyboardPoint {
                    x: focus.0,
                    y: focus.1,
                };
                if clipboard_available {
                    keyboard_input::preflight_transaction(
                        &input,
                        focus,
                        &[KeyboardEvent::Press("CTRL+V".into())],
                    )?;
                } else {
                    keyboard_input::preflight_paste(&input, focus, text).map(drop)?;
                }
            }
            GeneratedInputAction::KeyboardTransaction { .. }
            | GeneratedInputAction::Paste { .. } => {
                return Err(
                    "semantic-focus typing requires the focused-element input path".to_owned(),
                );
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
        *self
            .paste_delivery
            .lock()
            .expect("paste delivery mutex poisoned") = None;
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
        let clipboard = active.session.clipboard();
        let validated = ValidatedMapping::new(snapshot, mapping, &active.session, &active.stream)?;
        let points = action_png_points(&action)?;
        let stream_extent = StreamExtent::from_portal_stream(&mapping.stream);
        let resolved = input.resolved_region(
            mapping.output_size,
            stream_extent,
            mapping.window_crop_source.is_some(),
            &points,
        )?;
        let mapper = validated.eis_mapper(resolved.region)?;
        self.store_eis_evidence(resolved.evidence);
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
                GeneratedInputAction::KeyboardTransaction {
                    focus: KeyboardFocus::Point(focus),
                    events,
                } => {
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
                GeneratedInputAction::Paste {
                    focus: KeyboardFocus::Point(focus),
                    text,
                } => {
                    let focus = mapper.point(focus.x, focus.y)?;
                    let focus = KeyboardPoint {
                        x: focus.0,
                        y: focus.1,
                    };
                    let mechanism = perform_clipboard_paste(
                        clipboard,
                        input,
                        Some(focus),
                        text,
                        Arc::clone(&progress),
                    )
                    .await?;
                    *self
                        .paste_delivery
                        .lock()
                        .expect("paste delivery mutex poisoned") = Some(mechanism);
                }
                GeneratedInputAction::KeyboardTransaction { .. }
                | GeneratedInputAction::Paste { .. } => {
                    return Err(
                        "semantic-focus typing requires the focused-element input path".to_owned(),
                    );
                }
            }
            Ok(())
        }
        .await;
        result.map_err(InputError::from)
    }

    async fn prepare_focused_input<'a>(
        &'a self,
        action: &'a GeneratedInputAction,
    ) -> Result<(), String> {
        if !matches!(
            action,
            GeneratedInputAction::KeyboardTransaction {
                focus: KeyboardFocus::Semantic { .. },
                ..
            } | GeneratedInputAction::Paste {
                focus: KeyboardFocus::Semantic { .. },
                ..
            }
        ) {
            return Err("focused-element input requires semantic focus".into());
        }
        let mut state = self.state.lock().await;
        if state
            .active()
            .is_none_or(|active| active.session.is_closed())
        {
            exhaust_capture(
                &mut state,
                "portal session closed before focused input preparation",
            )
            .await;
            return Err(SESSION_UNAVAILABLE.into());
        }
        // No screenshot mapping is consulted: the target window may live on
        // a virtual desktop screenshots cannot see. EIS keystrokes land in
        // the focused window, so only a live session, keyboard capability,
        // and text bounds are required. The stream below only identifies the
        // portal session for EIS setup; no pixels are read from it.
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
            let stream = state
                .active()
                .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
                .stream
                .clone();
            match ReisInputBackend::connect(session, &stream).await {
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
        let input = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .input
            .as_ref()
            .ok_or_else(|| "EIS backend disappeared after setup".to_owned())?;
        let input = Arc::clone(input);
        // Focused-element typing needs only the unique keyboard: resolving a
        // pointer region here would let ambiguous multi-monitor EIS
        // advertisements block typing into a verified focused element.
        if let Err(error) = input.wait_for_keyboard().await {
            if state.active().and_then(terminal_failure).is_some() {
                exhaust_capture(&mut state, "desktop session failed while preparing input").await;
                return Err(SESSION_UNAVAILABLE.into());
            }
            return Err(error);
        }
        if let Some(failure) = state.active().and_then(terminal_failure) {
            exhaust_capture(&mut state, "desktop session failed while preparing input").await;
            return Err(format!("{SESSION_UNAVAILABLE}: {failure}"));
        }
        require_focused_capabilities(input.as_ref(), action)?;
        let clipboard_available = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .session
            .clipboard()
            .is_some();
        match action {
            GeneratedInputAction::KeyboardTransaction {
                focus: KeyboardFocus::Semantic { .. },
                events,
            } => {
                keyboard_input::preflight_transaction_focused(&input, events)?;
            }
            GeneratedInputAction::Paste {
                focus: KeyboardFocus::Semantic { .. },
                text,
            } => {
                if clipboard_available {
                    keyboard_input::preflight_transaction_focused(
                        &input,
                        &[KeyboardEvent::Press("CTRL+V".into())],
                    )?;
                } else {
                    keyboard_input::preflight_paste_focused(&input, text).map(drop)?;
                }
            }
            _ => {
                return Err("focused-element input requires semantic focus".into());
            }
        }
        Ok(())
    }

    async fn perform_focused_input(
        &self,
        action: GeneratedInputAction,
        progress: Arc<ActionProgress>,
    ) -> Result<(), InputError> {
        *self
            .paste_delivery
            .lock()
            .expect("paste delivery mutex poisoned") = None;
        let mut state = self.state.lock().await;
        if state
            .active()
            .is_none_or(|active| active.session.is_closed())
        {
            exhaust_capture(&mut state, "portal session closed before focused input").await;
            return Err(InputError::SessionUnavailable(SESSION_UNAVAILABLE.into()));
        }
        if let Some(failure) = state.active().and_then(terminal_failure) {
            exhaust_capture(&mut state, "desktop session failed before focused input").await;
            return Err(InputError::SessionUnavailable(format!(
                "{SESSION_UNAVAILABLE}: {failure}"
            )));
        }
        let input = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .input
            .as_ref()
            .ok_or_else(|| "EIS input was not prepared for this action".to_owned())?
            .clone();
        let clipboard = state
            .active()
            .ok_or_else(|| SESSION_UNAVAILABLE.to_owned())?
            .session
            .clipboard();
        require_focused_capabilities(input.as_ref(), &action)?;

        let result: Result<(), String> = async {
            match action {
                GeneratedInputAction::KeyboardTransaction {
                    focus: KeyboardFocus::Semantic { .. },
                    events,
                } => {
                    keyboard_input::perform_transaction_focused(
                        input,
                        events,
                        Arc::clone(&progress),
                    )
                    .await?;
                }
                GeneratedInputAction::Paste {
                    focus: KeyboardFocus::Semantic { .. },
                    text,
                } => {
                    let mechanism = perform_clipboard_paste(
                        clipboard,
                        input,
                        None,
                        text,
                        Arc::clone(&progress),
                    )
                    .await?;
                    *self
                        .paste_delivery
                        .lock()
                        .expect("paste delivery mutex poisoned") = Some(mechanism);
                }
                _ => {
                    return Err("focused-element input requires semantic focus".to_owned());
                }
            }
            Ok(())
        }
        .await;
        result.map_err(InputError::from)
    }

    async fn cleanup_input(&self) -> Result<(), String> {
        let (input, clipboard) = {
            let state = self.state.lock().await;
            let Some(active) = state.active() else {
                return Ok(());
            };
            (active.input.clone(), active.session.clipboard())
        };
        // Both are independently necessary, including when begin() or finish()
        // was dropped. A clipboard failure must not skip held-key release.
        let (input, clipboard) = tokio::join!(
            async {
                match input {
                    Some(input) => input.cleanup_barrier().await,
                    None => Ok(()),
                }
            },
            async {
                match clipboard {
                    Some(clipboard) => clipboard.clear_selection().await,
                    None => Ok(()),
                }
            },
        );
        match (input, clipboard) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(input), Err(clipboard)) => {
                Err(format!("{input}; clipboard cleanup failed: {clipboard}"))
            }
        }
    }

    async fn shutdown_input(&self) -> Result<(), String> {
        // Wake the initializer before waiting for its state lock. Dropping the
        // establishment future closes its request/session and stops retries.
        self.shutdown.send_replace(true);
        let active = take_active(&mut *self.state.lock().await);
        let Some(active) = active else {
            return Ok(());
        };
        close_active(active, "computer-use shutdown").await
    }

    fn eis_region_evidence(&self) -> Option<String> {
        self.eis_evidence
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn paste_delivery_mechanism(&self) -> Option<&'static str> {
        *self
            .paste_delivery
            .lock()
            .expect("paste delivery mutex poisoned")
    }
}

async fn perform_clipboard_paste(
    clipboard: Option<Arc<crate::portal::ClipboardController>>,
    input: Arc<ReisInputBackend>,
    focus: Option<KeyboardPoint>,
    text: String,
    progress: Arc<ActionProgress>,
) -> Result<&'static str, String> {
    let Some(clipboard) = clipboard else {
        return match focus {
            Some(focus) => keyboard_input::perform_paste(input, focus, text, progress)
                .await
                .map(|_| "eis_typed_insertion"),
            None => keyboard_input::perform_paste_focused(input, text, progress)
                .await
                .map(|_| "eis_typed_insertion"),
        };
    };
    let selection = clipboard.begin(&text, &progress).await?;
    let dispatch = match focus {
        Some(focus) => {
            keyboard_input::perform_transaction(
                input,
                focus,
                vec![KeyboardEvent::Press("CTRL+V".into())],
                progress,
            )
            .await
        }
        None => {
            keyboard_input::perform_transaction_focused(
                input,
                vec![KeyboardEvent::Press("CTRL+V".into())],
                progress,
            )
            .await
        }
    };
    // Completed progress refers to the Ctrl+V dispatch. A later transfer or
    // revocation failure remains an error, never a successful paste result.
    selection.finish(dispatch).await?;
    Ok("portal_clipboard")
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
        eprintln!(
            "computer-use-mcp: proceeding with degraded capture health; frame discontinuity risk — coordinates may misdeliver"
        );
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
    if mapping.crop == ObserveCrop::TargetWindow
        && (mapping.window_crop_source.is_none() || mapping.window_crop_geometry.is_none())
    {
        return Err(
            "target-window mapping has no authoritative KDE crop binding; re-observe".into(),
        );
    }
    if mapping.crop == ObserveCrop::Monitor
        && (mapping.window_crop_source.is_some() || mapping.window_crop_geometry.is_some())
    {
        return Err("monitor mapping retains a target crop binding; re-observe".into());
    }
    Ok(())
}

/// PNG points an action needs bound to EIS regions: every pointer target
/// plus the point-focus of keyboard actions. Semantic-focus typing takes the
/// coordinate-free focused path and never reaches here.
fn action_png_points(action: &GeneratedInputAction) -> Result<Vec<(f64, f64)>, String> {
    match action {
        GeneratedInputAction::Pointer(PointerAction::Move { x, y }) => Ok(vec![(*x, *y)]),
        GeneratedInputAction::Pointer(PointerAction::Click { x, y, .. }) => Ok(vec![(*x, *y)]),
        GeneratedInputAction::Pointer(PointerAction::Scroll { x, y, .. }) => Ok(vec![(*x, *y)]),
        GeneratedInputAction::Pointer(PointerAction::Drag { path }) => Ok(path.clone()),
        GeneratedInputAction::KeyboardTransaction {
            focus: KeyboardFocus::Point(focus),
            ..
        }
        | GeneratedInputAction::Paste {
            focus: KeyboardFocus::Point(focus),
            ..
        } => Ok(vec![(focus.x, focus.y)]),
        GeneratedInputAction::KeyboardTransaction { .. } | GeneratedInputAction::Paste { .. } => {
            Err("semantic-focus typing requires the focused-element input path".to_owned())
        }
    }
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
        GeneratedInputAction::KeyboardTransaction { .. } | GeneratedInputAction::Paste { .. } => {
            (false, false, true)
        }
    };
    backend.require_capabilities(button, scroll, keyboard)
}

/// Capability gate for focused-element typing: only the bound unique
/// keyboard is required, never a pointer region.
fn require_focused_capabilities(
    backend: &ReisInputBackend,
    action: &GeneratedInputAction,
) -> Result<(), String> {
    match action {
        GeneratedInputAction::KeyboardTransaction {
            focus: KeyboardFocus::Semantic { .. },
            ..
        }
        | GeneratedInputAction::Paste {
            focus: KeyboardFocus::Semantic { .. },
            ..
        } => backend.require_focused_keyboard(),
        _ => Err("focused-element input requires semantic focus".into()),
    }
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
        consent: Option<tokio::sync::watch::Receiver<bool>>,
    }

    impl PortalBackend for FakePortal {
        async fn establish(&self) -> Result<PortalConnection, String> {
            self.establishes.fetch_add(1, Ordering::AcqRel);
            if let Some(mut consent) = self.consent.clone() {
                consent
                    .wait_for(|ready| *ready)
                    .await
                    .map_err(|_| "consent gate closed".to_owned())?;
            }
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
        degraded_health: AtomicUsize,
        format_generation: AtomicUsize,
        current_metadata_checks: AtomicUsize,
        single_frame: AtomicUsize,
        renegotiate_after_baseline: AtomicUsize,
        fail_after_baseline: AtomicUsize,
        invalidated: AtomicUsize,
        rgba: StdMutex<Option<Vec<u8>>>,
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
                single_frame: self.0.single_frame.load(Ordering::Acquire) != 0,
            }))
        }
    }

    struct FakeCaptureSession {
        failure: Arc<StdMutex<Option<String>>>,
        drops: Arc<FakeCaptureState>,
        never_fresh: bool,
        single_frame: bool,
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
            if self.drops.invalidated.load(Ordering::Acquire) != 0 {
                return Ok(None);
            }
            Ok(Some(fake_metadata(
                1,
                self.drops.format_generation.load(Ordering::Acquire).max(1) as u64,
                fake_health(&self.drops),
            )))
        }

        fn latest_after(
            &mut self,
            after_generation: Option<u64>,
            _wait: Duration,
        ) -> CaptureFuture<'_, OwnedFrame> {
            let failure = self.failure();
            let never_fresh = self.never_fresh;
            let single_frame = self.single_frame;
            let health = fake_health(&self.drops);
            let rgba = self
                .drops
                .rgba
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| vec![255; 16]);
            let drops = Arc::clone(&self.drops);
            let failure_state = Arc::clone(&self.failure);
            Box::pin(async move {
                if let Some(error) = failure {
                    return Err(error);
                }
                if never_fresh {
                    return Err("timed out waiting for a complete frame".into());
                }
                if single_frame && after_generation.is_some() {
                    if drops.fail_after_baseline.swap(0, Ordering::AcqRel) != 0 {
                        *failure_state.lock().unwrap() = Some("stream disappeared".into());
                        return Err("stream disappeared".into());
                    }
                    if drops.renegotiate_after_baseline.swap(0, Ordering::AcqRel) != 0 {
                        drops.format_generation.store(2, Ordering::Release);
                        drops.invalidated.store(1, Ordering::Release);
                    }
                    return Err("timed out waiting for a complete frame".into());
                }
                let generation = after_generation.unwrap_or(0) + 1;
                Ok(OwnedFrame {
                    metadata: fake_metadata(generation, 1, health),
                    rgba,
                })
            })
        }
    }

    impl Drop for FakeCaptureSession {
        fn drop(&mut self) {
            self.drops.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn fake_health(state: &FakeCaptureState) -> crate::capture::StreamHealth {
        if state.failed_health.load(Ordering::Acquire) != 0 {
            crate::capture::StreamHealth::Failed
        } else if state.degraded_health.load(Ordering::Acquire) != 0 {
            crate::capture::StreamHealth::Degraded
        } else {
            crate::capture::StreamHealth::Healthy
        }
    }

    fn fake_metadata(
        generation: u64,
        format_generation: u64,
        health: crate::capture::StreamHealth,
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
            stream_health: health,
            content_hash: 0,
            change_epoch: 0,
            changed_from_previous: None,
            changed_rect: None,
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
                consent: None,
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
                    .contains("request a new approved desktop session")
            );
        }
    }

    #[tokio::test]
    async fn consent_concurrent_prepare_has_only_one_pending_prompt() {
        let (connection, _closed) = test_connection(1, 1);
        let mut coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        let (ready, receiver) = tokio::sync::watch::channel(false);
        coordinator.portal.consent = Some(receiver);
        let first = coordinator.prepare();
        let second = coordinator.prepare();
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
        assert!(!coordinator.desktop_session_exhausted());
        ready.send_replace(true);
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
        assert!(!coordinator.desktop_session_exhausted());
    }

    #[tokio::test]
    async fn consent_shutdown_cancels_pending_prompt_and_queued_prepare() {
        let mut coordinator = test_coordinator([], Arc::new(FakeCaptureState::default()));
        let (_ready, receiver) = tokio::sync::watch::channel(false);
        coordinator.portal.consent = Some(receiver);
        let first = coordinator.prepare();
        let second = coordinator.prepare();
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        let (first, second, shutdown) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(first, second, coordinator.shutdown_input())
        })
        .await
        .expect("shutdown must not wait behind the consent lock");
        assert!(first.unwrap_err().0.contains("shutdown"));
        assert!(second.unwrap_err().0.contains("shutdown"));
        shutdown.unwrap();
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
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
            crop: crate::validation::ObserveCrop::Monitor,
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
                changed_rect: None,
                sequence_gap: None,
            },
            output_size: (2, 2),
            crop: ObserveCrop::Monitor,
            window_crop_source: None,
            window_crop_geometry: None,
            window_crop_is_preencoded: false,
        }
    }

    #[tokio::test]
    async fn target_crop_wait_reuses_raw_view_and_input_mapping() {
        let (connection, _) = test_connection(1, 11);
        let capture = Arc::new(FakeCaptureState::default());
        *capture.rgba.lock().unwrap() = Some(vec![
            255, 0, 0, 255, 0, 255, 0, 255, // top row
            0, 0, 255, 255, 255, 255, 0, 255, // bottom row
        ]);
        let coordinator = test_coordinator([connection], Arc::clone(&capture));
        coordinator.prepare().await.unwrap();

        let mut snapshot = test_snapshot();
        snapshot.crop = ObserveCrop::TargetWindow;
        let geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: 1,
            height: 2,
            client: true,
        };
        let observation = coordinator
            .capture_with_window_geometry(&snapshot, Some(geometry))
            .await
            .unwrap();
        assert_eq!(observation.mapping.crop, ObserveCrop::TargetWindow);
        assert_eq!(
            observation.mapping.window_crop_source,
            Some(PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 2,
            })
        );
        assert_eq!(observation.mapping.window_crop_geometry, Some(geometry));
        assert!(observation.mapping.window_crop_is_preencoded);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&observation.png_base64)
            .unwrap();
        let image = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (1, 2));
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(0, 1).0, [0, 0, 255, 255]);

        let waited = coordinator
            .wait_for_frame(
                FrameWaitCondition::Advanced {
                    after_generation: observation.mapping.source.generation,
                },
                &observation.mapping,
            )
            .await
            .unwrap();
        assert_eq!(waited.mapping.output_size, (1, 2));
        assert_eq!(
            waited.mapping.window_crop_source,
            observation.mapping.window_crop_source
        );
        assert_eq!(
            waited.mapping.window_crop_geometry,
            observation.mapping.window_crop_geometry
        );
        let (session, _) = PortalSessionLease::for_test("/session/test", 1);
        let mapped = crate::input::coordinates::ValidatedMapping::new(
            &snapshot,
            &waited.mapping,
            &session,
            &waited.mapping.stream,
        )
        .unwrap()
        .eis_mapper(crate::input::coordinates::EisRegion {
            position: (0, 0),
            size: (2, 2),
            mapping_id: Some("mapping".into()),
        })
        .unwrap()
        .point(0.0, 1.0)
        .unwrap();
        assert_eq!(mapped, (0.0, 1.0));
    }

    #[tokio::test]
    async fn capture_reuses_one_current_complete_frame_after_startup_timeout() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.single_frame.store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();

        let observation = coordinator.capture(&test_snapshot()).await.unwrap();

        assert_eq!(observation.mapping.source.generation, 1);
        assert_eq!(observation.mapping.source.format_generation, 1);
        assert_eq!(
            capture_state
                .current_metadata_checks
                .load(Ordering::Acquire),
            1
        );
    }

    #[tokio::test]
    async fn capture_does_not_resurrect_baseline_after_format_renegotiation() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.single_frame.store(1, Ordering::Release);
        capture_state
            .renegotiate_after_baseline
            .store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();

        let error = coordinator.capture(&test_snapshot()).await.unwrap_err();

        assert!(error.0.contains("no current complete frame remains"));
        assert_eq!(
            capture_state
                .current_metadata_checks
                .load(Ordering::Acquire),
            1
        );
    }

    #[tokio::test]
    async fn capture_refuses_stream_failure_during_startup_fallback() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.single_frame.store(1, Ordering::Release);
        capture_state
            .fail_after_baseline
            .store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();

        let error = coordinator.capture(&test_snapshot()).await.unwrap_err();

        assert!(error.0.contains("request a new approved desktop session"));
        assert!(coordinator.desktop_session_exhausted());
        assert_eq!(
            capture_state
                .current_metadata_checks
                .load(Ordering::Acquire),
            0
        );
    }

    #[tokio::test]
    async fn failed_or_closed_session_requires_new_session_without_automatic_reprompt() {
        let (first, _) = test_connection(1, 11);
        let (second, _) = test_connection(2, 22);
        let capture_state = Arc::new(FakeCaptureState::default());
        let coordinator = test_coordinator([first, second], Arc::clone(&capture_state));

        assert!(!coordinator.desktop_session_exhausted());
        coordinator.prepare().await.unwrap();
        assert!(!coordinator.desktop_session_exhausted());
        assert_eq!(*capture_state.markers.lock().unwrap(), [11]);
        *capture_state.failures.lock().unwrap()[0].lock().unwrap() =
            Some("target node disappeared".into());
        assert!(coordinator.desktop_session_exhausted());
        assert!(coordinator.capture(&test_snapshot()).await.is_err());
        assert!(coordinator.desktop_session_exhausted());
        assert_terminal(&coordinator).await;
        assert_eq!(*capture_state.markers.lock().unwrap(), [11]);
        assert_eq!(capture_state.drops.load(Ordering::Acquire), 1);
        assert_eq!(coordinator.portal.establishes.load(Ordering::Acquire), 1);
        let (connection, closed) = test_connection(3, 33);
        let coordinator = test_coordinator([connection], Arc::new(FakeCaptureState::default()));
        coordinator.prepare().await.unwrap();
        closed.send_replace(true);
        assert!(coordinator.desktop_session_exhausted());
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
        assert!(!coordinator.desktop_session_exhausted());
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
        assert!(error.contains("request a new approved desktop session"));
        assert_terminal(&coordinator).await;
    }

    #[tokio::test]
    async fn degraded_capture_health_passes_current_validation_for_generated_input() {
        let (connection, _) = test_connection(1, 11);
        let capture_state = Arc::new(FakeCaptureState::default());
        capture_state.degraded_health.store(1, Ordering::Release);
        let coordinator = test_coordinator([connection], Arc::clone(&capture_state));
        coordinator.prepare().await.unwrap();

        let mut state = coordinator.state.lock().await;
        validate_current_capture_state(&mut state, &test_mapping(), "degraded health validation")
            .await
            .expect("degraded stream health is an approved downgrade, not a refusal");
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
            changed_rect: None,
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
}
