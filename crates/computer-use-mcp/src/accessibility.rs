use std::{
    collections::BTreeSet,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
use serde_json::{Value, json};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

use crate::{
    errors::{RuntimeError, StaleAuthority, ToolOutcome},
    input::{
        GeneratedInputAction,
        backend::{DispatchStage, InputError, PostStatus},
    },
    runtime::{
        ActionProgress, ActionProgressSnapshot, DesktopRuntime, ToolOutput, action_progress_json,
        compact_action_progress, with_action_progress, with_action_progress_snapshot,
    },
    screenshot::{
        FrameWaitCondition, NoScreenshots, SESSION_UNAVAILABLE, ScreenshotMapping,
        ScreenshotProvider,
    },
    validation::{
        AccessibilityRequest, AccessibilityScope, ActOperation, DEFAULT_ACCESSIBILITY_MAX_DEPTH,
        DEFAULT_ACCESSIBILITY_MAX_NODES, DEFAULT_ACCESSIBILITY_TEXT_LIMIT, DesktopScope,
        ElementAction, MAX_TEXT_LIMIT, ObservationRef, ObserveView, TargetRef, TextLimit, ToolCall,
        WaitCondition,
    },
    wayland_catalog::WaylandCatalog,
    window_backend::{
        AtspiBinding, BackendError, BackendKind, BackendStatus, CapabilityState, CatalogError,
        WindowCatalog, WindowEntry, WindowTarget,
    },
};

pub const EMPTY_APPS_MESSAGE: &str = "No running applications with accessible windows found.";

/// Hard budgets for model-facing observation/action payloads.  These are byte
/// budgets for UTF-8 text and serialized structured JSON respectively; the
/// implementation below trims complete elements or individual text fields,
/// never an already-serialized JSON byte slice.
pub const MAX_MODEL_TEXT_BYTES: usize = crate::runtime::MAX_MODEL_TEXT_BYTES;
pub const MAX_MODEL_STRUCTURED_BYTES: usize = crate::runtime::MAX_MODEL_STRUCTURED_BYTES;

const MAX_MODEL_FIELD_CHARS: usize = 1_024;
const MAX_MODEL_ACTIONS: usize = 16;
const MAX_MODEL_STATES: usize = 32;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ResponseTruncation {
    truncated: bool,
    fields_truncated: bool,
    elements_included: usize,
    elements_omitted: usize,
}

impl ResponseTruncation {
    fn json(self) -> Value {
        json!({
            "truncated": self.truncated,
            "reason": self.truncated.then_some("response_byte_budget"),
            "fields_truncated": self.fields_truncated,
            "elements_included": self.elements_included,
            "elements_omitted": self.elements_omitted,
        })
    }

    fn text_marker(self) -> String {
        format!(
            "Truncated: reason=response_byte_budget elements_included={} elements_omitted={} fields_truncated={}",
            self.elements_included, self.elements_omitted, self.fields_truncated
        )
    }
}

#[derive(Debug)]
struct TextProjection {
    text: String,
    truncation: ResponseTruncation,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId {
    pub bus_name: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    fn is_valid(self) -> bool {
        self.width >= 0 && self.height >= 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub object: ObjectId,
    pub title: String,
    pub states: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub object: ObjectId,
    pub name: String,
    pub pid: u32,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionCapabilities {
    Unsupported,
    InspectionFailed,
    Inspected(Vec<ActionInfo>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InspectedCapabilities {
    pub actions: ActionCapabilities,
    pub component: bool,
    pub editable_text: bool,
    pub value: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeCapabilities {
    InspectionFailed,
    Inspected(InspectedCapabilities),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetValueKind {
    Text,
    Number,
}

impl NodeCapabilities {
    fn actions(&self) -> &[ActionInfo] {
        self.inspected_actions().unwrap_or_default()
    }

    fn inspected_actions(&self) -> Result<&[ActionInfo], ()> {
        match self {
            Self::InspectionFailed
            | Self::Inspected(InspectedCapabilities {
                actions: ActionCapabilities::InspectionFailed,
                ..
            }) => Err(()),
            Self::Inspected(InspectedCapabilities {
                actions: ActionCapabilities::Unsupported,
                ..
            }) => Ok(&[]),
            Self::Inspected(InspectedCapabilities {
                actions: ActionCapabilities::Inspected(actions),
                ..
            }) => Ok(actions),
        }
    }

    fn inspection_complete(&self) -> bool {
        self.inspected_actions().is_ok()
    }

    fn set_value_kind(&self) -> Option<SetValueKind> {
        match self {
            Self::Inspected(InspectedCapabilities {
                editable_text: true,
                ..
            }) => Some(SetValueKind::Text),
            Self::Inspected(InspectedCapabilities { value: true, .. }) => {
                Some(SetValueKind::Number)
            }
            _ => None,
        }
    }

    fn interfaces_inspected(&self) -> bool {
        matches!(self, Self::Inspected(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub object: ObjectId,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub text: Option<String>,
    pub text_truncated: Option<bool>,
    pub selected_text: Option<String>,
    pub states: BTreeSet<String>,
    pub capabilities: NodeCapabilities,
    pub window_frame: Option<Rect>,
    pub children: Vec<ObjectId>,
}

impl NodeInfo {
    pub fn is_defunct(&self) -> bool {
        self.states.contains("defunct") || self.states.contains("stale")
    }

    fn supports_focus(&self) -> bool {
        self.states.contains("focusable")
            && matches!(
                &self.capabilities,
                NodeCapabilities::Inspected(InspectedCapabilities {
                    component: true,
                    ..
                })
            )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SemanticAction {
    InvokeAction(i32),
    GrabFocus,
    ReplaceText(String),
    SetNumericValue(f64),
}

pub trait AccessibilityAdapter: Send + Sync + 'static {
    fn discover(&self) -> impl Future<Output = Result<Vec<AppInfo>, RuntimeError>> + Send + '_;
    fn read_node<'a>(
        &'a self,
        object: &'a ObjectId,
        text_limit: usize,
    ) -> impl Future<Output = Result<NodeInfo, RuntimeError>> + Send + 'a;
    fn act<'a>(
        &'a self,
        object: &'a ObjectId,
        action: SemanticAction,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'a;
    fn activate<'a>(
        &'a self,
        _object: &'a ObjectId,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'a {
        async {
            Err(capability_error(
                "the accessibility backend does not advertise window activation",
            ))
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeConfig {
    pub default_max_nodes: usize,
    pub default_max_depth: usize,
    pub default_text_limit: usize,
    pub call_timeout: Duration,
    pub portal_timeout: Duration,
    pub snapshot_timeout: Duration,
    pub settle_interval: Duration,
}

struct DesktopSession {
    status: watch::Sender<Option<Result<(), RuntimeError>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for DesktopSession {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            default_max_nodes: DEFAULT_ACCESSIBILITY_MAX_NODES,
            default_max_depth: DEFAULT_ACCESSIBILITY_MAX_DEPTH,
            default_text_limit: DEFAULT_ACCESSIBILITY_TEXT_LIMIT,
            call_timeout: Duration::from_secs(2),
            portal_timeout: Duration::from_secs(60),
            snapshot_timeout: Duration::from_secs(12),
            settle_interval: Duration::from_millis(150),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementSnapshot {
    pub depth: usize,
    pub node: NodeInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub text: usize,
    pub nodes: usize,
    pub depth: usize,
}

#[derive(Debug, Clone)]
struct VisualSnapshotOptions {
    view: ObserveView,
    accessibility_scope: AccessibilityScope,
    element_query: Option<String>,
    limits: SnapshotLimits,
    accessibility_ready: bool,
    accessibility_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub view: AccessibilityScope,
    pub element_query: Option<String>,
    pub app: AppInfo,
    pub window: WindowInfo,
    pub generation: u64,
    pub elements: Vec<ElementSnapshot>,
    pub element_ids: Vec<String>,
    pub node_limit_reached: bool,
    pub depth_limit_reached: bool,
    pub limits: SnapshotLimits,
    pub target_ref: Option<TargetRef>,
    pub accessibility_ready: bool,
    pub accessibility_reason: Option<String>,
    pub requires_atspi_revalidation: bool,
    pub screenshot_requested: bool,
}

struct ElementWaitRequest<'a, F> {
    target: &'a TargetRef,
    baseline: Arc<Snapshot>,
    element_id: &'a str,
    deadline: tokio::time::Instant,
    condition: WaitCondition,
    predicate: F,
    success_message: &'a str,
}

struct GeneratedActionResult {
    output: ToolOutput,
    replacement: Option<Arc<Snapshot>>,
}

#[derive(Debug)]
struct CachedObservation {
    snapshot: Arc<Snapshot>,
    screenshot_mapping: Option<ScreenshotMapping>,
}

#[derive(Debug, Default)]
struct Cache {
    generation: u64,
    element_generation: u64,
    observations: Vec<CachedObservation>,
}

const MAX_CACHED_OBSERVATIONS: usize = 8;
const MAX_CACHED_SNAPSHOT_STRING_BYTES: usize = 8 * 1024 * 1024;

impl Cache {
    fn insert(&mut self, mut snapshot: Snapshot) -> Result<Arc<Snapshot>, RuntimeError> {
        if snapshot_string_bytes(&snapshot) > MAX_CACHED_SNAPSHOT_STRING_BYTES {
            return Err(operational_error(
                "observation exceeds the retained-state byte limit; lower text_limit or max_tree_nodes",
            ));
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| operational_error("snapshot generation overflow"))?;
        snapshot.generation = self.generation;
        let mut element_ids = Vec::with_capacity(snapshot.elements.len());
        for _ in &snapshot.elements {
            let id = format!("e-{:016x}", self.element_generation);
            self.element_generation = self
                .element_generation
                .checked_add(1)
                .ok_or_else(|| operational_error("element ID generation overflow"))?;
            element_ids.push(id);
        }
        snapshot.element_ids = element_ids;
        let snapshot = Arc::new(snapshot);
        self.observations
            .retain(|cached| !same_target(&cached.snapshot, &snapshot));
        self.observations.push(CachedObservation {
            snapshot: Arc::clone(&snapshot),
            screenshot_mapping: None,
        });
        while self.observations.len() > MAX_CACHED_OBSERVATIONS
            || self
                .observations
                .iter()
                .map(|cached| snapshot_string_bytes(&cached.snapshot))
                .sum::<usize>()
                > MAX_CACHED_SNAPSHOT_STRING_BYTES
        {
            self.observations.remove(0);
        }
        Ok(snapshot)
    }

    fn required(&self, observation_id: &str) -> Result<&CachedObservation, RuntimeError> {
        self.observations
            .iter()
            .find(|cached| observation_id_for_snapshot(&cached.snapshot) == observation_id)
            .ok_or_else(|| {
                if self.observations.is_empty() {
                    state_required_error("no observation is available; call observe first")
                } else {
                    stale_observation_error(format!("observation_id {observation_id:?} is stale"))
                }
            })
    }

    fn position(&self, expected: &Snapshot) -> Result<usize, RuntimeError> {
        let position = self
            .observations
            .iter()
            .position(|cached| cached.snapshot.generation == expected.generation)
            .ok_or_else(|| {
                state_required_error("state cache lost the observation before action dispatch")
            })?;
        if !same_target(&self.observations[position].snapshot, expected) {
            return Err(stale_observation_error(
                "state changed before action dispatch; call observe again",
            ));
        }
        Ok(position)
    }

    fn invalidate_for_mutation(&mut self, expected: &Snapshot) -> Result<(), RuntimeError> {
        let position = self.position(expected)?;
        self.observations.remove(position);
        self.clear_screenshot_mappings();
        Ok(())
    }

    fn clear_screenshot_mappings(&mut self) {
        for cached in &mut self.observations {
            cached.screenshot_mapping = None;
        }
    }

    fn invalidate_all(&mut self) {
        self.observations.clear();
    }

    fn frame_for_target(
        &self,
        target: &TargetRef,
        frame_id: Option<&str>,
    ) -> Result<(Arc<Snapshot>, ScreenshotMapping), RuntimeError> {
        self.observations
            .iter()
            .rev()
            .filter(|cached| cached.snapshot.target_ref.as_ref() == Some(target))
            .find_map(|cached| {
                cached.screenshot_mapping.as_ref().and_then(|mapping| {
                    frame_id
                        .is_none_or(|frame_id| crate::capture::frame_id(&mapping.source) == frame_id)
                        .then(|| (Arc::clone(&cached.snapshot), mapping.clone()))
                })
            })
            .ok_or_else(|| {
                if frame_id.is_some() {
                    stale_observation_error(
                        "frame_id is unknown, stale, or belongs to another target",
                    )
                } else {
                    state_required_error(
                        "no retained monitor frame belongs to this target; call observe with screenshot",
                    )
                }
            })
    }
}

fn same_target(left: &Snapshot, right: &Snapshot) -> bool {
    left.app.pid == right.app.pid
        && left.app.object == right.app.object
        && left.window.object == right.window.object
}

fn object_string_bytes(object: &ObjectId) -> usize {
    object.bus_name.len() + object.path.len()
}

fn window_string_bytes(window: &WindowInfo) -> usize {
    object_string_bytes(&window.object)
        + window.title.len()
        + window.states.iter().map(String::len).sum::<usize>()
        + window.states.len() * std::mem::size_of::<String>()
}

fn app_string_bytes(app: &AppInfo) -> usize {
    object_string_bytes(&app.object)
        + app.name.len()
        + app.windows.iter().map(window_string_bytes).sum::<usize>()
        + std::mem::size_of_val(app.windows.as_slice())
}

fn node_string_bytes(node: &NodeInfo) -> usize {
    object_string_bytes(&node.object)
        + node.role.len()
        + node.name.len()
        + node.value.as_ref().map_or(0, String::len)
        + node.text.as_ref().map_or(0, String::len)
        + node.selected_text.as_ref().map_or(0, String::len)
        + node.states.iter().map(String::len).sum::<usize>()
        + node
            .capabilities
            .actions()
            .iter()
            .map(|action| action.name.len() + action.description.len())
            .sum::<usize>()
        + node.children.iter().map(object_string_bytes).sum::<usize>()
        + node.states.len() * std::mem::size_of::<String>()
        + std::mem::size_of_val(node.capabilities.actions())
        + std::mem::size_of_val(node.children.as_slice())
}

fn snapshot_string_bytes(snapshot: &Snapshot) -> usize {
    snapshot.element_query.as_ref().map_or(0, String::len)
        + app_string_bytes(&snapshot.app)
        + window_string_bytes(&snapshot.window)
        + snapshot
            .elements
            .iter()
            .map(|element| node_string_bytes(&element.node))
            .sum::<usize>()
}

pub struct SemanticRuntime<A, S = NoScreenshots> {
    adapter: A,
    screenshots: Arc<S>,
    desktop_session: DesktopSession,
    config: RuntimeConfig,
    cache: Mutex<Cache>,
    catalog: Mutex<WindowCatalog>,
    wayland: Arc<WaylandCatalog>,
    mutation: tokio::sync::Mutex<()>,
    launch_in_progress: Arc<AtomicBool>,
    launch_tasks: Arc<crate::desktop_launcher::LaunchTasks>,
}

impl<A, S> std::fmt::Debug for SemanticRuntime<A, S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SemanticRuntime")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<A: AccessibilityAdapter> SemanticRuntime<A, NoScreenshots> {
    pub fn new(adapter: A) -> Self {
        Self::with_config(adapter, RuntimeConfig::default())
    }

    pub fn with_config(adapter: A, config: RuntimeConfig) -> Self {
        Self::with_screenshot_provider(adapter, NoScreenshots, config)
    }
}

impl<A: AccessibilityAdapter, S: ScreenshotProvider> SemanticRuntime<A, S> {
    pub fn with_screenshot_provider(adapter: A, screenshots: S, config: RuntimeConfig) -> Self {
        let wayland = Arc::new(WaylandCatalog::start());
        Self {
            adapter,
            screenshots: Arc::new(screenshots),
            desktop_session: DesktopSession {
                status: watch::channel(None).0,
                task: Mutex::new(None),
            },
            config,
            cache: Mutex::new(Cache::default()),
            catalog: Mutex::new(WindowCatalog::default()),
            wayland,
            mutation: tokio::sync::Mutex::new(()),
            launch_in_progress: Arc::new(AtomicBool::new(false)),
            launch_tasks: Arc::new(crate::desktop_launcher::LaunchTasks::default()),
        }
    }

    fn start_desktop_session(&self) {
        let mut task = self
            .desktop_session
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.is_some() || self.desktop_session.status.borrow().is_some() {
            return;
        }

        let screenshots = Arc::clone(&self.screenshots);
        let status = self.desktop_session.status.clone();
        let portal_timeout = self.config.portal_timeout;
        *task = Some(tokio::spawn(async move {
            let result = AssertUnwindSafe(async {
                timeout(portal_timeout, screenshots.prepare())
                    .await
                    .map_err(|_| {
                        desktop_session_error(
                            "backend_timeout",
                            "desktop session initialization timed out",
                        )
                    })?
                    .map_err(|error| {
                        desktop_session_error(
                            "backend_failed",
                            format!("KDE RemoteDesktop approval failed: {error}"),
                        )
                    })
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                eprintln!("computer-use-mcp: desktop session initializer panicked");
                Err(desktop_session_error(
                    "backend_failed",
                    "desktop session initializer panicked",
                ))
            });
            status.send_if_modified(|current| {
                if current.is_some() {
                    return false;
                }
                *current = Some(result);
                true
            });
        }));
    }

    async fn desktop_session(&self) -> Result<(), RuntimeError> {
        self.start_desktop_session();
        let mut status = self.desktop_session.status.subscribe();
        status
            .wait_for(Option::is_some)
            .await
            .expect("desktop session status sender is retained by the runtime")
            .clone()
            .expect("desktop session status was checked as present")
    }

    async fn stop_desktop_session(&self) {
        let task = {
            let mut task = self
                .desktop_session
                .task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.desktop_session
                .status
                .send_replace(Some(Err(desktop_session_error(
                    "backend_failed",
                    "desktop session has shut down",
                ))));
            task.take()
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    async fn list_desktop(
        &self,
        scope: DesktopScope,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ToolOutput, RuntimeError> {
        match scope {
            DesktopScope::Applications => {
                let apps = crate::desktop_launcher::list_installed_apps().await?;
                let generation = installed_app_page_generation(&apps);
                let start = page_start(cursor, scope, generation, apps.len())?;
                let end = start
                    .checked_add(limit)
                    .ok_or_else(|| operational_error("desktop page limit overflow"))?
                    .min(apps.len());
                let page = &apps[start..end];
                let next_cursor = (end < apps.len()).then(|| page_cursor(scope, generation, end));
                let text = if page.is_empty() {
                    "No installed desktop applications found.".to_owned()
                } else {
                    let entries = page
                        .iter()
                        .map(|app| {
                            format!(
                                "{} — {}",
                                truncate(&escape(&app.name), MAX_MODEL_FIELD_CHARS),
                                app.desktop_id
                            )
                        })
                        .collect::<Vec<_>>();
                    bounded_list_text("", &entries, next_cursor.as_deref())
                };
                let text = if page.is_empty() {
                    bounded_list_text(&text, &[], next_cursor.as_deref())
                } else {
                    text
                };
                let structured = json!({
                    "scope": "applications",
                    "limit": limit,
                    "next_cursor": next_cursor,
                    "applications": page.iter().map(|app| json!({
                        "desktop_id": app.desktop_id,
                        "name": app.name,
                        "shown": app.shown,
                        "capabilities": {"launch": "supported"}
                    })).collect::<Vec<_>>()
                });
                Ok(ToolOutput::text(text).with_structured_content(structured))
            }
            DesktopScope::Windows => {
                let apps = match self.discover().await {
                    Ok(apps) => apps,
                    Err(error) => {
                        eprintln!(
                            "computer-use-mcp: AT-SPI discovery unavailable while listing compositor windows: {error}"
                        );
                        Vec::new()
                    }
                };
                let compositor = self.wayland.snapshot().await.map_err(|error| {
                    eprintln!(
                        "computer-use-mcp: Wayland catalog unavailable while listing windows: {error}"
                    );
                    operational_error(format!("Wayland catalog snapshot failed: {error}"))
                })?;
                let (entries, membership_generation) = {
                    let mut catalog = self.lock_catalog()?;
                    let entries = catalog
                        .reconcile_sources(&apps, compositor.records)
                        .map_err(catalog_error)?;
                    (entries, catalog.membership_generation())
                };
                let start = page_start(cursor, scope, membership_generation, entries.len())?;
                let end = start
                    .checked_add(limit)
                    .ok_or_else(|| operational_error("desktop page limit overflow"))?
                    .min(entries.len());
                let page = &entries[start..end];
                let next_cursor =
                    (end < entries.len()).then(|| page_cursor(scope, membership_generation, end));
                let backend_text = format!(
                    "Backends: standard_foreign_toplevel={} kde_plasma_rich={}",
                    compact_backend_status(&compositor.standard),
                    compact_backend_status(&compositor.kde),
                );
                let text = if page.is_empty() {
                    bounded_list_text(
                        &backend_text,
                        &[EMPTY_APPS_MESSAGE.to_owned()],
                        next_cursor.as_deref(),
                    )
                } else {
                    let entries = page.iter().map(compact_window_entry).collect::<Vec<_>>();
                    bounded_list_text(&backend_text, &entries, next_cursor.as_deref())
                };
                let structured = json!({
                    "scope": "windows",
                    "limit": limit,
                    "next_cursor": next_cursor,
                    "windows": page.iter().map(WindowEntry::as_json).collect::<Vec<_>>(),
                    "backends": {
                        "standard_foreign_toplevel": compositor.standard.capability().as_json(),
                        "kde_plasma_rich": compositor.kde.capability().as_json(),
                    }
                });
                Ok(ToolOutput::text(text).with_structured_content(structured))
            }
        }
    }

    fn lock_catalog(&self) -> Result<std::sync::MutexGuard<'_, WindowCatalog>, RuntimeError> {
        self.catalog.lock().map_err(|_| {
            eprintln!("computer-use-mcp: window catalog mutex poisoned");
            operational_error("window catalog invariant failed")
        })
    }

    fn target_entry(&self, target: &TargetRef) -> Result<WindowEntry, RuntimeError> {
        self.lock_catalog()?
            .get(&WindowTarget {
                app_instance_id: target.app_instance_id.clone(),
                window_instance_id: target.window_instance_id.clone(),
            })
            .map_err(catalog_error)
    }

    async fn refresh_window_catalog(&self) -> Result<(), RuntimeError> {
        let apps = match self.discover().await {
            Ok(apps) => apps,
            Err(error) => {
                eprintln!(
                    "computer-use-mcp: AT-SPI discovery unavailable while refreshing target catalog: {error}"
                );
                Vec::new()
            }
        };
        let compositor = self.wayland.snapshot().await.map_err(|error| {
            eprintln!(
                "computer-use-mcp: Wayland catalog unavailable while refreshing targets: {error}"
            );
            operational_error(format!("Wayland catalog snapshot failed: {error}"))
        })?;
        self.lock_catalog()?
            .reconcile_sources(&apps, compositor.records)
            .map_err(catalog_error)?;
        Ok(())
    }

    async fn requested_target_snapshot(
        &self,
        target: &TargetRef,
        view: ObserveView,
        accessibility: Option<AccessibilityRequest>,
    ) -> Result<Arc<Snapshot>, RuntimeError> {
        // Target IDs are process-lifetime handles, but the backend record must
        // still be present in the current catalog before visual capture or
        // accessibility collection begins.
        self.refresh_window_catalog().await?;
        let entry = self.target_entry(target)?;
        let accessibility = accessibility.unwrap_or_else(|| AccessibilityRequest {
            scope: AccessibilityScope::default(),
            query: None,
            limits: crate::validation::AccessibilityLimits {
                text: TextLimit::Count(self.config.default_text_limit),
                nodes: self.config.default_max_nodes,
                depth: self.config.default_max_depth,
            },
        });
        let wants_accessibility = matches!(view, ObserveView::Accessibility | ObserveView::Both);
        let wants_screenshot = matches!(view, ObserveView::Screenshot | ObserveView::Both);
        let snapshot_limits = SnapshotLimits {
            text: match accessibility.limits.text {
                TextLimit::Count(value) => value,
                TextLimit::Max => MAX_TEXT_LIMIT,
            },
            nodes: accessibility.limits.nodes,
            depth: accessibility.limits.depth,
        };

        if !wants_accessibility {
            return self.commit_snapshot(self.visual_snapshot(
                &entry,
                target,
                VisualSnapshotOptions {
                    view,
                    accessibility_scope: accessibility.scope,
                    element_query: accessibility.query.clone(),
                    limits: snapshot_limits,
                    accessibility_ready: false,
                    accessibility_reason: Some("accessibility was not requested".into()),
                },
            ));
        }

        let Some(binding) = entry.atspi.clone() else {
            let reason = "this window has no AT-SPI authority";
            if wants_screenshot {
                return self.commit_snapshot(self.visual_snapshot(
                    &entry,
                    target,
                    VisualSnapshotOptions {
                        view,
                        accessibility_scope: accessibility.scope,
                        element_query: accessibility.query.clone(),
                        limits: snapshot_limits,
                        accessibility_ready: false,
                        accessibility_reason: Some(reason.into()),
                    },
                ));
            }
            return self.commit_snapshot(self.visual_snapshot(
                &entry,
                target,
                VisualSnapshotOptions {
                    view,
                    accessibility_scope: accessibility.scope,
                    element_query: accessibility.query,
                    limits: snapshot_limits,
                    accessibility_ready: false,
                    accessibility_reason: Some(reason.into()),
                },
            ));
        };
        let mut snapshot = self
            .collect_snapshot(
                &binding,
                accessibility.scope,
                accessibility.query,
                snapshot_limits,
            )
            .await?;
        snapshot.target_ref = Some(target.clone());
        snapshot.screenshot_requested = wants_screenshot;
        snapshot.requires_atspi_revalidation = true;
        self.commit_snapshot(snapshot)
    }

    fn visual_snapshot(
        &self,
        entry: &WindowEntry,
        target: &TargetRef,
        options: VisualSnapshotOptions,
    ) -> Snapshot {
        let (app, window) = entry.atspi.clone().map_or_else(
            || {
                let app_object = ObjectId {
                    bus_name: "visual-authority".into(),
                    path: format!("/{}", target.app_instance_id),
                };
                let window_object = ObjectId {
                    bus_name: "visual-authority".into(),
                    path: format!("/{}", target.window_instance_id),
                };
                (
                    AppInfo {
                        object: app_object,
                        name: entry.app_id.clone().unwrap_or_else(|| "unknown".into()),
                        pid: entry.pid.unwrap_or_default(),
                        windows: Vec::new(),
                    },
                    WindowInfo {
                        object: window_object,
                        title: entry.title.clone(),
                        states: entry.states.clone(),
                    },
                )
            },
            |binding| (binding.app, binding.window),
        );
        Snapshot {
            view: options.accessibility_scope,
            element_query: options.element_query,
            app,
            window,
            generation: 0,
            elements: Vec::new(),
            element_ids: Vec::new(),
            node_limit_reached: false,
            depth_limit_reached: false,
            limits: options.limits,
            target_ref: Some(target.clone()),
            accessibility_ready: options.accessibility_ready,
            accessibility_reason: options.accessibility_reason,
            requires_atspi_revalidation: false,
            screenshot_requested: matches!(
                options.view,
                ObserveView::Screenshot | ObserveView::Both
            ),
        }
    }

    async fn activate_window(
        &self,
        target: &TargetRef,
        progress: &ActionProgress,
    ) -> Result<ToolOutput, RuntimeError> {
        self.refresh_window_catalog().await?;
        let entry = self.target_entry(target)?;
        let before_active = entry.states.contains("active");
        if entry.source != BackendKind::KdePlasma && entry.source != BackendKind::Atspi {
            return Err(capability_error(
                "this compositor authority does not advertise activation",
            ));
        }
        if entry.source == BackendKind::Atspi && entry.atspi.is_none() {
            return Err(capability_error(
                "window activation is unavailable because this target has no verified AT-SPI binding",
            ));
        }
        self.lock_cache()?.invalidate_all();

        let activation_deadline = tokio::time::Instant::now() + self.config.call_timeout;
        let (
            backend,
            status,
            after_active,
            transition,
            atspi_active_observed,
            protocol_state_verified,
            request_accepted,
            protocol_request_sent,
            request_flushed,
        ) = if entry.source == BackendKind::KdePlasma {
            if before_active {
                (
                    "kde-plasma",
                    "already_active",
                    true,
                    "already_active",
                    false,
                    false,
                    false,
                    false,
                    false,
                )
            } else {
                let pending = self
                    .wayland
                    .begin_activation(entry.backend_identity.clone(), self.config.call_timeout)
                    .map_err(backend_activation_error);
                let activation = match pending {
                    Ok(pending) => {
                        progress.mark_started();
                        pending.wait().await.map_err(backend_activation_error)
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = activation {
                    if error.outcome != ToolOutcome::NotStarted
                        && progress.snapshot().dispatch_stage == DispatchStage::NotStarted
                    {
                        progress.mark_started();
                    }
                    return Err(with_action_progress_snapshot(error, progress.snapshot()));
                }
                progress.mark_completed();
                (
                    "kde-plasma",
                    "protocol_state_verified",
                    true,
                    "active_transition_observed",
                    false,
                    true,
                    true,
                    true,
                    true,
                )
            }
        } else {
            let binding = entry.atspi.clone().ok_or_else(|| {
                capability_error(
                    "window activation is unavailable because this target has no verified AT-SPI binding",
                )
            })?;
            progress.mark_started();
            let dispatch = timeout(
                self.config.call_timeout,
                self.adapter.activate(&binding.window.object),
            )
            .await
            .map_err(|_| timeout_error("AT-SPI activation dispatch timed out"))
            .and_then(|result| result);
            if let Err(error) = dispatch {
                return Err(map_attempt_error(error, progress.snapshot()));
            }
            progress.mark_completed();

            let mut after_active = false;
            while tokio::time::Instant::now() < activation_deadline {
                let remaining =
                    activation_deadline.saturating_duration_since(tokio::time::Instant::now());
                let apps = match timeout(remaining, self.adapter.discover()).await {
                    Ok(Ok(apps)) => apps,
                    Ok(Err(error)) => {
                        progress.mark_post_accessibility(PostStatus::AccessibilityRefreshFailed);
                        return Err(with_action_progress_snapshot(
                            completed_without_observation(error),
                            progress.snapshot(),
                        ));
                    }
                    Err(_) => break,
                };
                if let Some(window) = apps
                    .iter()
                    .find(|app| app.object == binding.app.object && app.pid == binding.app.pid)
                    .and_then(|app| {
                        app.windows
                            .iter()
                            .find(|window| window.object == binding.window.object)
                    })
                {
                    after_active = window.states.contains("active");
                    if after_active {
                        break;
                    }
                }
                let remaining =
                    activation_deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                sleep(Duration::from_millis(25).min(remaining)).await;
            }
            if !after_active {
                progress.mark_post_accessibility(PostStatus::Unavailable);
                return Err(with_action_progress_snapshot(
                    RuntimeError::new(
                        "activation_unknown",
                        "AT-SPI activation was dispatched but no fresh active-state evidence was observed",
                        ToolOutcome::Completed,
                        false,
                        "Call list_desktop and observe the exact target before deciding whether activation is still needed; do not retry blindly.",
                    ),
                    progress.snapshot(),
                ));
            }
            (
                "atspi",
                "atspi_active_observed",
                after_active,
                if before_active {
                    "already_active"
                } else {
                    "active_state_observed"
                },
                true,
                false,
                true,
                true,
                false,
            )
        };

        if let Err(error) = self.refresh_window_catalog().await {
            progress.mark_post_accessibility(post_status_for_error(&error, true));
            return Err(with_action_progress_snapshot(
                completed_without_observation(error),
                progress.snapshot(),
            ));
        }
        let replacement = self
            .requested_target_snapshot(target, ObserveView::Accessibility, None)
            .await
            .map_err(|error| {
                progress.mark_post_accessibility(post_status_for_error(&error, true));
                with_action_progress_snapshot(
                    completed_without_observation(error),
                    progress.snapshot(),
                )
            })?;
        progress.mark_post_accessibility(post_accessibility_status(&replacement));
        let replacement = self.observe(replacement, Ok(())).await.map_err(|error| {
            progress.mark_post_accessibility(post_status_for_error(&error, true));
            with_action_progress_snapshot(completed_without_observation(error), progress.snapshot())
        })?;
        let replacement_structured = replacement.structured_content.unwrap_or(Value::Null);
        let replacement_observation_id = replacement_structured["observation_id"]
            .as_str()
            .map(str::to_owned);
        let text = format!(
            "Activation: backend={backend} status={status} before_active={before_active} after_active={after_active} transition={transition} request_accepted={request_accepted} protocol_request_sent={protocol_request_sent} request_flushed={request_flushed} seat_focus=not_observable replacement_observation={replacement_observation_id:?}"
        );
        let structured = json!({
            "outcome": "completed",
            "target": target.as_json(),
            "before_active": before_active,
            "after_active": after_active,
            "status": status,
            "transition": transition,
            "atspi_active_observed": atspi_active_observed,
            "protocol_state_verified": protocol_state_verified,
            "seat_focus": "not_observable",
            "dispatch": {
                "request_accepted": request_accepted,
                "protocol_request_sent": protocol_request_sent,
                "request_flushed": request_flushed,
                "synchronized": false,
                "client_delivery": "not_observable"
            },
            "backend": backend,
            "replacement_observation_id": replacement_observation_id,
            "replacement_observation": replacement_structured,
        });
        Ok(ToolOutput::text(text)
            .with_structured_content(bound_action_structured(structured))
            .with_action_progress(progress))
    }

    async fn act_new(
        &self,
        target: &TargetRef,
        source: &ObservationRef,
        operation: ActOperation,
        progress: Arc<ActionProgress>,
    ) -> Result<ToolOutput, RuntimeError> {
        self.refresh_window_catalog().await?;
        let observation_id = self.observation_id_for_source(source, target)?;
        let source_snapshot = self.required_cached(&observation_id)?;
        // Refresh first, then resolve the exact opaque target. The catalog
        // guarantees that this ID remains bound to one backend object,
        // authority, and PID for its entire lifetime.
        let _ = self.target_entry(target)?;
        let (output, replacement_snapshot) = match operation.clone() {
            ActOperation::Semantic { element_id, action } => {
                let snapshot = self
                    .element_action(&observation_id, &element_id, action, progress.as_ref())
                    .await?;
                let output = self
                    .post_action_observation(Arc::clone(&snapshot), Arc::clone(&progress))
                    .await?;
                (output, Some(snapshot))
            }
            ActOperation::Pointer { action } => {
                let Some(frame_id) = &source.frame_id else {
                    return Err(state_required_error(
                        "spatial act requires frame_id from the exact source observation",
                    ));
                };
                let mapping = self.required_screenshot(&observation_id)?.1;
                if crate::capture::frame_id(&mapping.source) != *frame_id {
                    eprintln!(
                        "computer-use-mcp: refusing spatial action with a mismatched source frame: expected={} supplied={frame_id}",
                        crate::capture::frame_id(&mapping.source)
                    );
                    return Err(stale_observation_error(
                        "source frame is stale; call observe again",
                    ));
                }
                let output = self
                    .perform_generated_with_progress(
                        &observation_id,
                        GeneratedInputAction::Pointer(action),
                        Arc::clone(&progress),
                    )
                    .await?;
                (output.output, output.replacement)
            }
            ActOperation::Keyboard { focus, events } => {
                let Some(frame_id) = &source.frame_id else {
                    return Err(state_required_error(
                        "keyboard act requires frame_id from the exact source observation",
                    ));
                };
                let mapping = self.required_screenshot(&observation_id)?.1;
                if crate::capture::frame_id(&mapping.source) != *frame_id {
                    return Err(stale_observation_error(
                        "source frame is stale; call observe again",
                    ));
                }
                let output = self
                    .perform_generated_with_progress(
                        &observation_id,
                        GeneratedInputAction::KeyboardTransaction { focus, events },
                        Arc::clone(&progress),
                    )
                    .await?;
                (output.output, output.replacement)
            }
        };
        Ok(annotate_action_output_with_progress(
            output,
            target,
            source,
            &operation,
            &source_snapshot,
            replacement_snapshot.as_deref(),
            Some(progress.as_ref()),
        ))
    }

    fn observation_id_for_source(
        &self,
        source: &ObservationRef,
        target: &TargetRef,
    ) -> Result<String, RuntimeError> {
        parse_opaque_counter(&source.observation_id, "obs")?;
        let snapshot = self.required_cached(&source.observation_id)?;
        if snapshot.target_ref.as_ref() != Some(target) {
            eprintln!("computer-use-mcp: source observation target does not match act target");
            return Err(stale_observation_error(
                "source observation belongs to another target",
            ));
        }
        Ok(source.observation_id.clone())
    }

    async fn wait_for_new(
        &self,
        target: &TargetRef,
        condition: WaitCondition,
        timeout_ms: u64,
    ) -> Result<ToolOutput, RuntimeError> {
        let _ = self.target_entry(target)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        match condition.clone() {
            WaitCondition::FrameAdvanced { after_frame_id } => {
                let (baseline, mapping) = self
                    .lock_cache()?
                    .frame_for_target(target, Some(&after_frame_id))?;
                let after_generation = mapping.source.generation;
                self.wait_for_frame_condition(
                    target,
                    baseline,
                    mapping,
                    condition,
                    timeout_ms,
                    FrameWaitCondition::Advanced { after_generation },
                    "a later capture frame was acquired",
                )
                .await
            }
            WaitCondition::FrameChanged { after_frame_id } => {
                let (baseline, mapping) = self
                    .lock_cache()?
                    .frame_for_target(target, Some(&after_frame_id))?;
                let after_generation = mapping.source.generation;
                let after_change_epoch = mapping.source.change_epoch;
                self.wait_for_frame_condition(
                    target,
                    baseline,
                    mapping,
                    condition,
                    timeout_ms,
                    FrameWaitCondition::Changed {
                        after_generation,
                        after_change_epoch,
                    },
                    "a later capture frame reported content change evidence",
                )
                .await
            }
            WaitCondition::FrameStable { for_ms } => {
                let (baseline, mapping) = self.lock_cache()?.frame_for_target(target, None)?;
                let after_generation = mapping.source.generation;
                let after_change_epoch = mapping.source.change_epoch;
                let after_format_generation = mapping.source.format_generation;
                self.wait_for_frame_condition(
                    target,
                    baseline,
                    mapping,
                    condition,
                    timeout_ms,
                    FrameWaitCondition::Stable {
                        after_generation,
                        after_change_epoch,
                        after_format_generation,
                        for_duration: Duration::from_millis(for_ms),
                    },
                    "capture content remained stable for the requested interval",
                )
                .await
            }
            WaitCondition::AccessibilityAdvanced {
                after_observation_id,
            } => {
                let baseline = self.wait_baseline(&after_observation_id, target)?;
                self.wait_for_accessibility_change(target, baseline, deadline, condition)
                    .await
            }
            WaitCondition::ElementState {
                observation_id,
                element_id,
                state,
            } => {
                let baseline = self.wait_baseline(&observation_id, target)?;
                cached_element(&baseline, &element_id)?;
                self.wait_for_element_condition(ElementWaitRequest {
                    target,
                    baseline,
                    element_id: &element_id,
                    deadline,
                    condition,
                    predicate: move |node: &NodeInfo| node.states.contains(&state),
                    success_message:
                        "the later accessibility observation reported the requested element state",
                })
                .await
            }
            WaitCondition::ElementValue {
                observation_id,
                element_id,
                value,
            } => {
                let baseline = self.wait_baseline(&observation_id, target)?;
                cached_element(&baseline, &element_id)?;
                self.wait_for_element_condition(ElementWaitRequest {
                    target,
                    baseline,
                    element_id: &element_id,
                    deadline,
                    condition,
                    predicate: move |node: &NodeInfo| node.value.as_deref() == Some(value.as_str()),
                    success_message:
                        "the later accessibility observation reported the requested element value",
                })
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn wait_for_frame_condition(
        &self,
        target: &TargetRef,
        baseline: Arc<Snapshot>,
        mapping: ScreenshotMapping,
        condition: WaitCondition,
        timeout_ms: u64,
        frame_condition: FrameWaitCondition,
        success_message: &str,
    ) -> Result<ToolOutput, RuntimeError> {
        let result = timeout(
            Duration::from_millis(timeout_ms),
            self.screenshots.wait_for_frame(frame_condition, &mapping),
        )
        .await;
        let evidence = match result {
            Err(_) => {
                return Ok(wait_output(
                    target,
                    condition,
                    false,
                    "no matching capture frame before the deadline",
                ));
            }
            Ok(Err(error)) if error.0.to_lowercase().contains("timed out") => {
                return Ok(wait_output(
                    target,
                    condition,
                    false,
                    "no matching capture frame before the deadline",
                ));
            }
            Ok(Err(error)) => return Err(wait_backend_error(&error.0)),
            Ok(Ok(evidence)) => evidence,
        };
        if evidence.mapping.app_pid != mapping.app_pid
            || evidence.mapping.app_identity != mapping.app_identity
            || evidence.mapping.window_identity != mapping.window_identity
            || evidence.mapping.accessibility_generation != mapping.accessibility_generation
            || evidence.mapping.portal_session_identity != mapping.portal_session_identity
            || evidence.mapping.portal_session_generation != mapping.portal_session_generation
            || evidence.mapping.stream != mapping.stream
            || evidence.mapping.source.generation <= mapping.source.generation
            || evidence.mapping.output_size.0 == 0
            || evidence.mapping.output_size.1 == 0
        {
            eprintln!("computer-use-mcp: frame wait returned an unbound or stale visual mapping");
            return Err(stale_observation_error(
                "frame wait returned a visual binding that is stale for the exact target",
            ));
        }
        self.cache_screenshot(&baseline, evidence.mapping.clone())?;
        Ok(wait_output_with_evidence(
            target,
            condition,
            true,
            success_message,
            WaitEvidence {
                frame: Some(&evidence.mapping.source),
                observation_id: None,
                dimensions: Some(evidence.mapping.output_size),
                changed: evidence.changed,
                stable_for_ms: evidence.stable_for_ms,
            },
        ))
    }

    fn wait_baseline(
        &self,
        observation_id: &str,
        target: &TargetRef,
    ) -> Result<Arc<Snapshot>, RuntimeError> {
        parse_opaque_counter(observation_id, "obs")?;
        let snapshot = self.required_cached(observation_id)?;
        if snapshot.target_ref.as_ref() != Some(target) {
            return Err(stale_observation_error(
                "wait condition observation belongs to another target",
            ));
        }
        Ok(snapshot)
    }

    async fn wait_for_accessibility_change(
        &self,
        target: &TargetRef,
        baseline: Arc<Snapshot>,
        deadline: tokio::time::Instant,
        condition: WaitCondition,
    ) -> Result<ToolOutput, RuntimeError> {
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(wait_output(
                    target,
                    condition,
                    false,
                    "no accessibility change before the deadline",
                ));
            }
            let current = match timeout(
                remaining,
                self.collect_exact_snapshot(&baseline, baseline.limits),
            )
            .await
            {
                Err(_) => {
                    return Ok(wait_output(
                        target,
                        condition,
                        false,
                        "no accessibility change before the deadline",
                    ));
                }
                Ok(result) => self.commit_snapshot(result?)?,
            };
            if !same_snapshot_content(&baseline, &current) {
                let current_observation_id = observation_id_for_snapshot(&current);
                return Ok(wait_output_with_evidence(
                    target,
                    condition,
                    true,
                    "a later AT-SPI observation reported changed accessibility content",
                    WaitEvidence {
                        frame: None,
                        observation_id: Some(&current_observation_id),
                        dimensions: None,
                        changed: None,
                        stable_for_ms: None,
                    },
                ));
            }
            sleep(Duration::from_millis(25).min(remaining)).await;
        }
    }

    async fn wait_for_element_condition<F>(
        &self,
        request: ElementWaitRequest<'_, F>,
    ) -> Result<ToolOutput, RuntimeError>
    where
        F: Fn(&NodeInfo) -> bool,
    {
        let ElementWaitRequest {
            target,
            baseline,
            element_id,
            deadline,
            condition,
            predicate,
            success_message,
        } = request;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(wait_output(
                    target,
                    condition,
                    false,
                    "element condition was not satisfied before the deadline",
                ));
            }
            let current = match timeout(
                remaining,
                self.collect_exact_snapshot(&baseline, baseline.limits),
            )
            .await
            {
                Err(_) => {
                    return Ok(wait_output(
                        target,
                        condition,
                        false,
                        "element condition was not satisfied before the deadline",
                    ));
                }
                Ok(result) => self.commit_snapshot(result?)?,
            };
            let element = match relocated_element(&baseline, &current, element_id) {
                Ok(element) => element,
                Err(error) if error.code == "target_unavailable" => return Err(error),
                Err(error) => return Err(error),
            };
            if predicate(&element.node) {
                let current_observation_id = observation_id_for_snapshot(&current);
                return Ok(wait_output_with_evidence(
                    target,
                    condition,
                    true,
                    success_message,
                    WaitEvidence {
                        frame: None,
                        observation_id: Some(&current_observation_id),
                        dimensions: None,
                        changed: None,
                        stable_for_ms: None,
                    },
                ));
            }
            sleep(Duration::from_millis(25).min(remaining)).await;
        }
    }

    #[cfg(test)]
    async fn execute_call(&self, call: ToolCall) -> Result<ToolOutput, RuntimeError> {
        let progress = call
            .tracks_action()
            .then(|| Arc::new(ActionProgress::default()));
        self.execute_call_with_progress(call, progress).await
    }

    async fn execute_call_with_progress(
        &self,
        call: ToolCall,
        progress: Option<Arc<ActionProgress>>,
    ) -> Result<ToolOutput, RuntimeError> {
        call.validate_policy()?;
        let visual_session = if call.requires_visual_session() {
            self.desktop_session().await
        } else {
            Ok(())
        };
        match call {
            ToolCall::ListDesktop {
                scope,
                limit,
                cursor,
            } => {
                let _mutation = self.mutation.lock().await;
                self.list_desktop(scope, limit, cursor.as_deref()).await
            }
            ToolCall::LaunchApplication { desktop_id } => {
                let _mutation = self.mutation.lock().await;
                self.invalidate_for_launch()?;
                let Some(progress) = progress else {
                    eprintln!(
                        "computer-use-mcp: launch call reached runtime without an attempt record"
                    );
                    return Err(operational_error("launch attempt record is missing"));
                };
                let launched = crate::desktop_launcher::launch(
                    &desktop_id,
                    Arc::clone(&self.launch_in_progress),
                    Arc::clone(&self.launch_tasks),
                    Arc::clone(&progress),
                )
                .await
                .map_err(|error| map_attempt_error(error, progress.snapshot()))?;
                Ok(ToolOutput::text(format!(
                    "Launch requested for {} (desktop_id={}).",
                    launched.name, launched.desktop_id
                ))
                .with_structured_content(json!({
                    "status": "requested",
                    "desktop_id": launched.desktop_id,
                    "name": launched.name
                })))
                .map(|output| output.with_action_progress(progress.as_ref()))
            }
            ToolCall::ActivateWindow { target } => {
                let _mutation = self.mutation.lock().await;
                if self.launch_in_progress.load(Ordering::Acquire) {
                    return Err(launch_in_progress_error());
                }
                let Some(progress) = progress else {
                    eprintln!(
                        "computer-use-mcp: activation call reached runtime without an attempt record"
                    );
                    return Err(operational_error("activation attempt record is missing"));
                };
                self.activate_window(&target, progress.as_ref()).await
            }
            ToolCall::Observe {
                target,
                view,
                accessibility,
            } => {
                let _mutation = self.mutation.lock().await;
                if self.launch_in_progress.load(Ordering::Acquire) {
                    return Err(launch_in_progress_error());
                }
                let snapshot = self
                    .requested_target_snapshot(&target, view, accessibility)
                    .await?;
                self.observe(snapshot, visual_session).await
            }
            ToolCall::Act {
                target,
                source,
                operation,
            } => {
                let _mutation = self.mutation.lock().await;
                if self.launch_in_progress.load(Ordering::Acquire) {
                    return Err(launch_in_progress_error());
                }
                let Some(progress) = progress else {
                    eprintln!(
                        "computer-use-mcp: act call reached runtime without an attempt record"
                    );
                    return Err(operational_error("act attempt record is missing"));
                };
                self.act_new(&target, &source, operation, progress).await
            }
            ToolCall::WaitFor {
                target,
                condition,
                timeout_ms,
            } => {
                let _mutation = self.mutation.lock().await;
                if self.launch_in_progress.load(Ordering::Acquire) {
                    return Err(launch_in_progress_error());
                }
                self.wait_for_new(&target, condition, timeout_ms).await
            }
        }
    }

    async fn discover(&self) -> Result<Vec<AppInfo>, RuntimeError> {
        timeout(self.config.call_timeout, self.adapter.discover())
            .await
            .map_err(|_| operational_error("AT-SPI application discovery timed out"))?
    }

    async fn read_node(&self, id: &ObjectId, text_limit: usize) -> Result<NodeInfo, RuntimeError> {
        timeout(
            self.config.call_timeout,
            self.adapter.read_node(id, text_limit),
        )
        .await
        .map_err(|_| {
            operational_error(format!(
                "AT-SPI call timed out while reading {}{}",
                id.bus_name, id.path
            ))
        })?
    }

    fn commit_snapshot(&self, snapshot: Snapshot) -> Result<Arc<Snapshot>, RuntimeError> {
        self.lock_cache()?.insert(snapshot)
    }

    fn invalidate_for_launch(&self) -> Result<(), RuntimeError> {
        // A launch can change any visible state, so observations and frame
        // mappings are stale. Existing target handles remain valid while their
        // exact backend objects remain present and must never be reissued.
        self.lock_cache()?.invalidate_all();
        Ok(())
    }

    async fn collect_snapshot(
        &self,
        binding: &AtspiBinding,
        view: AccessibilityScope,
        element_query: Option<String>,
        limits: SnapshotLimits,
    ) -> Result<Snapshot, RuntimeError> {
        let apps = self.discover().await?;
        let matching_apps = apps
            .into_iter()
            .filter(|app| app.pid == binding.app.pid && app.object == binding.app.object)
            .collect::<Vec<_>>();
        let [app] = matching_apps.as_slice() else {
            return Err(operational_error(
                "application PID or object identity changed since the exact catalog binding",
            ));
        };
        let matching_windows = app
            .windows
            .iter()
            .filter(|window| window.object == binding.window.object)
            .cloned()
            .collect::<Vec<_>>();
        let [window] = matching_windows.as_slice() else {
            return Err(operational_error(
                "window object identity changed since the exact catalog binding",
            ));
        };
        if !window_is_viable(window) {
            return Err(operational_error(format!(
                "matched window {:?} is stale or defunct",
                window.title
            )));
        }
        let metadata_bytes = element_query.as_ref().map_or(0, String::len)
            + app_string_bytes(app)
            + window_string_bytes(window);
        let element_budget = MAX_CACHED_SNAPSHOT_STRING_BYTES
            .checked_sub(metadata_bytes)
            .ok_or_else(|| {
                operational_error(
                    "observation metadata exceeds the retained-state byte limit; use a narrower target",
                )
            })?;
        let elements = self.traverse(window, view, limits, element_budget).await?;
        Ok(Snapshot {
            view,
            element_query,
            app: app.clone(),
            window: window.clone(),
            generation: 0,
            node_limit_reached: elements.node_limit_reached,
            depth_limit_reached: elements.depth_limit_reached,
            elements: elements.elements,
            element_ids: Vec::new(),
            limits,
            target_ref: None,
            accessibility_ready: true,
            accessibility_reason: None,
            requires_atspi_revalidation: true,
            screenshot_requested: true,
        })
    }

    async fn traverse(
        &self,
        window: &WindowInfo,
        view: AccessibilityScope,
        limits: SnapshotLimits,
        string_byte_budget: usize,
    ) -> Result<Traversal, RuntimeError> {
        let mut stack = vec![(window.object.clone(), 0_usize)];
        let mut elements = Vec::new();
        let mut node_limit_reached = false;
        let mut depth_limit_reached = false;
        let mut string_bytes = 0_usize;
        while let Some((object, depth)) = stack.pop() {
            if elements.len() >= limits.nodes {
                node_limit_reached = true;
                break;
            }
            let mut node = match self.read_node(&object, limits.text).await {
                Ok(node) => node,
                Err(error) if depth > 0 => {
                    eprintln!(
                        "computer-use-mcp: skipping stale AT-SPI child: object={}{} error={error}",
                        object.bus_name, object.path
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            if node.object != object {
                eprintln!("computer-use-mcp: AT-SPI adapter returned mismatched object identity");
                return Err(operational_error(
                    "AT-SPI object identity changed while reading",
                ));
            }
            if node.is_defunct() {
                if depth == 0 {
                    return Err(operational_error(format!(
                        "selected window {}{} is defunct or stale",
                        object.bus_name, object.path
                    )));
                }
                eprintln!(
                    "computer-use-mcp: skipping defunct AT-SPI child: object={}{}",
                    object.bus_name, object.path
                );
                continue;
            }
            if depth > 0 && view != AccessibilityScope::Full && is_hidden_document(&node) {
                continue;
            }
            string_bytes = string_bytes
                .checked_add(node_string_bytes(&node))
                .filter(|bytes| *bytes <= string_byte_budget)
                .ok_or_else(|| {
                    operational_error(
                        "observation exceeds the retained-state byte limit; lower text_limit or max_tree_nodes",
                    )
                })?;
            node.window_frame = normalize_frame(&node);
            let children = node.children.clone();
            elements.push(ElementSnapshot { depth, node });
            if depth >= limits.depth {
                if !children.is_empty() {
                    depth_limit_reached = true;
                }
                continue;
            }
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
        Ok(Traversal {
            elements,
            node_limit_reached,
            depth_limit_reached,
        })
    }

    async fn element_action(
        &self,
        observation_id: &str,
        index: &str,
        action: ElementAction,
        progress: &ActionProgress,
    ) -> Result<Arc<Snapshot>, RuntimeError> {
        let cached = self
            .required_cached(observation_id)
            .map_err(|error| with_action_progress(error, progress))?;
        let old_element = cached_element(&cached, index)
            .map_err(|error| with_action_progress(error, progress))?;
        match self
            .read_node(&old_element.node.object, cached.limits.text)
            .await
        {
            Ok(node) if node.is_defunct() => {
                return Err(operational_error("target element is defunct"));
            }
            Ok(_) => {}
            Err(error) => eprintln!(
                "computer-use-mcp: cached AT-SPI object is stale; attempting strict relocation: {error}"
            ),
        }
        let current = self
            .fresh_for_action(&cached)
            .await
            .map_err(|error| with_action_progress(error, progress))?;
        let target = relocate(old_element, &current.elements)
            .map_err(|error| with_action_progress(error, progress))?;
        ensure_element_presented(&current, target, index)
            .map_err(|error| with_action_progress(error, progress))?;
        let semantic = semantic_action(action, target)
            .map_err(|error| with_action_progress(error, progress))?;
        self.lock_cache()
            .map_err(|error| with_action_progress(error, progress))?
            .invalidate_for_mutation(&cached)
            .map_err(|error| with_action_progress(error, progress))?;
        progress.mark_started();
        let dispatch = timeout(
            self.config.call_timeout,
            self.adapter.act(&target.node.object, semantic),
        )
        .await
        .map_err(|_| timeout_error("AT-SPI semantic action timed out"))
        .and_then(|result| result);
        match dispatch {
            Ok(()) => {
                progress.mark_completed();
            }
            Err(error) => return Err(map_attempt_error(error, progress.snapshot())),
        }
        match self.settle_and_refresh(cached).await {
            Ok(snapshot) => {
                progress.mark_post_accessibility(post_accessibility_status(&snapshot));
                Ok(snapshot)
            }
            Err(error) => {
                progress.mark_post_accessibility(post_status_for_error(&error, true));
                Err(with_action_progress(
                    completed_without_observation(error),
                    progress,
                ))
            }
        }
    }

    async fn fresh_for_action(&self, cached: &Snapshot) -> Result<Snapshot, RuntimeError> {
        if !cached.requires_atspi_revalidation {
            let Some(target) = cached.target_ref.as_ref() else {
                eprintln!(
                    "computer-use-mcp: visual-only action snapshot has no exact target identity"
                );
                return Err(operational_error(
                    "visual action snapshot has no exact target; call observe again",
                ));
            };
            self.refresh_window_catalog().await?;
            let entry = self.target_entry(target)?;
            if !entry.capabilities.screenshot.is_supported() {
                return Err(capability_error(
                    "the exact target no longer advertises screenshot input",
                ));
            }
            return Ok(cached.clone());
        }
        let limits = SnapshotLimits {
            text: cached.limits.text,
            nodes: cached.limits.nodes,
            depth: cached.limits.depth,
        };
        let current = self.collect_exact_snapshot(cached, limits).await?;
        if current.app.object != cached.app.object {
            return Err(operational_error(
                "application identity changed since the prior state",
            ));
        }
        Ok(current)
    }

    async fn collect_exact_snapshot(
        &self,
        cached: &Snapshot,
        limits: SnapshotLimits,
    ) -> Result<Snapshot, RuntimeError> {
        let mut snapshot = self
            .collect_snapshot(
                &AtspiBinding {
                    app: cached.app.clone(),
                    window: cached.window.clone(),
                },
                cached.view,
                cached.element_query.clone(),
                limits,
            )
            .await?;
        snapshot.target_ref = cached.target_ref.clone();
        snapshot.screenshot_requested = cached.screenshot_requested;
        snapshot.accessibility_ready = cached.accessibility_ready;
        snapshot.accessibility_reason = cached.accessibility_reason.clone();
        snapshot.requires_atspi_revalidation = cached.requires_atspi_revalidation;
        Ok(snapshot)
    }

    async fn perform_generated_with_progress(
        &self,
        observation_id: &str,
        action: GeneratedInputAction,
        progress: Arc<ActionProgress>,
    ) -> Result<GeneratedActionResult, RuntimeError> {
        let (cached, mapping) = self
            .required_screenshot(observation_id)
            .map_err(|error| with_action_progress(error, progress.as_ref()))?;
        self.fresh_for_action(&cached)
            .await
            .map_err(|error| with_action_progress(error, progress.as_ref()))?;
        self.lock_cache()?
            .position(&cached)
            .map_err(|error| with_action_progress(error, progress.as_ref()))?;
        let preparation = timeout(
            self.config.snapshot_timeout,
            self.screenshots.prepare_input(&cached, &mapping, &action),
        )
        .await;
        let cleanup = self.screenshots.cleanup_input().await;
        if cleanup.is_err() {
            progress.mark_cleanup_failed();
        } else {
            progress.mark_cleanup_completed();
        }
        let preparation = preparation
            .map_err(|_| operational_error("generated input preparation timed out"))
            .map_err(|error| with_action_progress(error, progress.as_ref()))?
            .map_err(generated_input_error);
        if let Err(error) = preparation {
            if let Err(cleanup) = cleanup {
                eprintln!(
                    "computer-use-mcp: cleanup also failed after input preparation error: {cleanup}"
                );
                return Err(with_action_progress(
                    operational_error(format!(
                        "{error}; generated input cleanup also failed and the input session was invalidated: {cleanup}"
                    )),
                    progress.as_ref(),
                ));
            }
            return Err(with_action_progress(error, progress.as_ref()));
        }
        cleanup
            .map_err(|error| {
                operational_error(format!(
                    "generated input preparation cleanup failed: {error}"
                ))
            })
            .map_err(|error| with_action_progress(error, progress.as_ref()))?;
        self.fresh_for_action(&cached)
            .await
            .map_err(|error| with_action_progress(error, progress.as_ref()))?;
        self.lock_cache()?
            .invalidate_for_mutation(&cached)
            .map_err(|error| with_action_progress(error, progress.as_ref()))?;
        let result = timeout(
            self.config.snapshot_timeout,
            self.screenshots
                .perform_input(&cached, &mapping, action, Arc::clone(&progress)),
        )
        .await;
        let cleanup = self.screenshots.cleanup_input().await;
        if cleanup.is_err() {
            progress.mark_cleanup_failed();
        } else if progress.snapshot().dispatch_stage != DispatchStage::NotStarted {
            progress.mark_cleanup_completed();
        }
        let snapshot = progress.snapshot();
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) if snapshot.dispatch_stage == DispatchStage::Completed => {
                return Err(completed_dispatch_error(
                    input_runtime_error(&error),
                    snapshot,
                ));
            }
            Ok(Err(error)) => {
                return Err(map_input_error(error, snapshot));
            }
            Err(_) if snapshot.dispatch_stage == DispatchStage::Completed => {}
            Err(_) => {
                return Err(map_attempt_error(
                    timeout_error("generated input action timed out"),
                    snapshot,
                ));
            }
        }
        if let Err(cleanup) = cleanup {
            let error = operational_error(format!("generated input cleanup failed: {cleanup}"));
            return Err(if snapshot.dispatch_stage == DispatchStage::Completed {
                completed_cleanup_error(error, snapshot)
            } else {
                map_attempt_error(error, snapshot)
            });
        }
        if snapshot.dispatch_stage != DispatchStage::Completed {
            return Err(with_action_progress(
                operational_error("generated input returned before completing its dispatch"),
                progress.as_ref(),
            ));
        }
        let output = self
            .post_generated_action(Arc::clone(&cached), Arc::clone(&progress))
            .await?;
        Ok(output)
    }

    async fn post_generated_action(
        &self,
        cached: Arc<Snapshot>,
        progress: Arc<ActionProgress>,
    ) -> Result<GeneratedActionResult, RuntimeError> {
        let refreshed = match self.settle_and_refresh(Arc::clone(&cached)).await {
            Ok(refreshed) => {
                progress.mark_post_accessibility(post_accessibility_status(&refreshed));
                refreshed
            }
            Err(error) => {
                progress.mark_post_accessibility(post_status_for_error(&error, true));
                progress.mark_post_visual(PostStatus::NotRun);
                return Err(with_action_progress_snapshot(
                    completed_without_observation(error),
                    progress.snapshot(),
                ));
            }
        };
        let output = self
            .post_action_observation(Arc::clone(&refreshed), Arc::clone(&progress))
            .await?;
        Ok(GeneratedActionResult {
            output,
            replacement: Some(refreshed),
        })
    }

    async fn post_action_observation(
        &self,
        refreshed: Arc<Snapshot>,
        progress: Arc<ActionProgress>,
    ) -> Result<ToolOutput, RuntimeError> {
        let visual_requested = refreshed.screenshot_requested;
        match self
            .observe_with_status(Arc::clone(&refreshed), Ok(()))
            .await
        {
            Ok((output, visual_status)) => {
                if visual_requested {
                    progress.mark_post_visual(visual_status);
                    if visual_status != PostStatus::Observed {
                        let reason = output
                            .structured_content
                            .as_ref()
                            .and_then(|value| value["screenshot"]["reason"].as_str());
                        self.discard_failed_post_observation(&refreshed);
                        return Err(with_action_progress_snapshot(
                            completed_post_visual_failure(visual_status, reason),
                            progress.snapshot(),
                        ));
                    }
                }
                if output
                    .structured_content
                    .as_ref()
                    .is_some_and(|value| value["accessibility"]["ready"] == true)
                {
                    progress.mark_post_accessibility(PostStatus::Observed);
                }
                Ok(output)
            }
            Err(error) => {
                if visual_requested {
                    let visual_status = post_visual_error_status(&error);
                    progress.mark_post_visual(visual_status);
                    self.discard_failed_post_observation(&refreshed);
                    return Err(with_action_progress_snapshot(
                        completed_post_visual_failure(visual_status, Some(&error.message)),
                        progress.snapshot(),
                    ));
                }
                self.discard_failed_post_observation(&refreshed);
                Err(with_action_progress_snapshot(
                    completed_without_observation(error),
                    progress.snapshot(),
                ))
            }
        }
    }

    fn discard_failed_post_observation(&self, snapshot: &Snapshot) {
        match self.lock_cache() {
            Ok(mut cache) => {
                cache
                    .observations
                    .retain(|cached| cached.snapshot.generation != snapshot.generation);
                cache.clear_screenshot_mappings();
            }
            Err(error) => eprintln!(
                "computer-use-mcp: failed to discard unusable post-action observation: {error}"
            ),
        }
    }

    async fn settle_and_refresh(&self, old: Arc<Snapshot>) -> Result<Arc<Snapshot>, RuntimeError> {
        sleep(self.config.settle_interval).await;
        self.refresh_window_catalog().await?;
        let refreshed = if let Some(target) = old.target_ref.as_ref() {
            let entry = self
                .target_entry(target)
                .map_err(completed_without_observation)?;
            let future = self.replacement_snapshot(&old, target, &entry);
            timeout(self.config.snapshot_timeout, future)
                .await
                .map_err(|_| operational_error("snapshot timed out after action"))??
        } else {
            let future = self.collect_exact_snapshot(&old, old.limits);
            timeout(self.config.snapshot_timeout, future)
                .await
                .map_err(|_| operational_error("AT-SPI snapshot timed out after action"))??
        };
        if refreshed.app.object != old.app.object {
            return Err(operational_error(
                "application identity changed while settling after the action",
            ));
        }
        self.commit_snapshot(refreshed)
    }

    async fn replacement_snapshot(
        &self,
        old: &Snapshot,
        target: &TargetRef,
        entry: &WindowEntry,
    ) -> Result<Snapshot, RuntimeError> {
        if !old.requires_atspi_revalidation {
            let view = if old.screenshot_requested {
                ObserveView::Screenshot
            } else {
                ObserveView::Accessibility
            };
            return Ok(self.visual_snapshot(
                entry,
                target,
                VisualSnapshotOptions {
                    view,
                    accessibility_scope: old.view,
                    element_query: old.element_query.clone(),
                    limits: old.limits,
                    accessibility_ready: old.accessibility_ready && entry.atspi.is_some(),
                    accessibility_reason: old.accessibility_reason.clone(),
                },
            ));
        }

        let Some(binding) = entry.atspi.as_ref() else {
            let view = if old.screenshot_requested {
                ObserveView::Both
            } else {
                ObserveView::Accessibility
            };
            return Ok(self.visual_snapshot(
                entry,
                target,
                VisualSnapshotOptions {
                    view,
                    accessibility_scope: old.view,
                    element_query: old.element_query.clone(),
                    limits: old.limits,
                    accessibility_ready: false,
                    accessibility_reason: Some(
                        "the target no longer has a verified AT-SPI binding".into(),
                    ),
                },
            ));
        };
        let mut snapshot = self
            .collect_snapshot(binding, old.view, old.element_query.clone(), old.limits)
            .await?;
        snapshot.target_ref = Some(target.clone());
        snapshot.screenshot_requested = old.screenshot_requested;
        snapshot.requires_atspi_revalidation = true;
        Ok(snapshot)
    }

    async fn observe(
        &self,
        snapshot: Arc<Snapshot>,
        desktop_session: Result<(), RuntimeError>,
    ) -> Result<ToolOutput, RuntimeError> {
        self.observe_with_status(snapshot, desktop_session)
            .await
            .map(|(output, _)| output)
    }

    async fn observe_with_status(
        &self,
        snapshot: Arc<Snapshot>,
        desktop_session: Result<(), RuntimeError>,
    ) -> Result<(ToolOutput, PostStatus), RuntimeError> {
        if !snapshot.screenshot_requested {
            return Ok((
                observation_output(
                    &snapshot,
                    false,
                    Some("screenshot was not requested"),
                    None,
                    None,
                    None,
                ),
                PostStatus::NotRequested,
            ));
        }
        if snapshot.target_ref.is_some() {
            // Refresh immediately before capture so a target removed after
            // requested_target_snapshot cannot start a visual operation.
            self.revalidate_screenshot_target(&snapshot).await?;
        }
        if let Err(error) = desktop_session {
            eprintln!(
                "computer-use-mcp: screenshot preparation failed for pid={} window={}{} generation={}: {error}",
                snapshot.app.pid,
                snapshot.window.object.bus_name,
                snapshot.window.object.path,
                snapshot.generation
            );
            return Ok((
                screenshot_unavailable(&snapshot, &error.to_string()),
                post_visual_error_status(&error),
            ));
        }
        match timeout(
            self.config.snapshot_timeout,
            self.screenshots.capture(&snapshot),
        )
        .await
        {
            Ok(Ok(observation)) => {
                if observation.mapping.app_pid != snapshot.app.pid
                    || observation.mapping.app_identity != snapshot.app.object
                    || observation.mapping.window_identity != snapshot.window.object
                    || observation.mapping.accessibility_generation != snapshot.generation
                {
                    eprintln!(
                        "computer-use-mcp: screenshot mapping identity invariant failed: snapshot_pid={} mapping_pid={} snapshot_generation={} mapping_generation={}",
                        snapshot.app.pid,
                        observation.mapping.app_pid,
                        snapshot.generation,
                        observation.mapping.accessibility_generation
                    );
                    return Ok((
                        screenshot_unavailable(
                            &snapshot,
                            "screenshot mapping identity changed during capture",
                        ),
                        PostStatus::CaptureFailed,
                    ));
                }
                if let Err(error) = self.revalidate_screenshot_target(&snapshot).await {
                    eprintln!(
                        "computer-use-mcp: screenshot target changed after frame acquisition for pid={}: {error}",
                        snapshot.app.pid
                    );
                    if snapshot.target_ref.is_some() {
                        return Err(error);
                    }
                    return Ok((
                        screenshot_unavailable(&snapshot, &error.to_string()),
                        PostStatus::CaptureFailed,
                    ));
                }
                if let Err(error) = self.cache_screenshot(&snapshot, observation.mapping.clone()) {
                    eprintln!(
                        "computer-use-mcp: screenshot cache update failed for pid={}: {error}",
                        snapshot.app.pid
                    );
                    return Ok((
                        screenshot_unavailable(&snapshot, &error.to_string()),
                        PostStatus::CaptureFailed,
                    ));
                }
                let (width, height) = observation.mapping.output_size;
                Ok((
                    observation_output(
                        &snapshot,
                        true,
                        None,
                        Some((width, height)),
                        Some(observation.png_base64),
                        Some(&observation.mapping.source),
                    ),
                    PostStatus::Observed,
                ))
            }
            Ok(Err(error)) => {
                eprintln!(
                    "computer-use-mcp: screenshot unavailable for pid={} window={}{} generation={}: {error}",
                    snapshot.app.pid,
                    snapshot.window.object.bus_name,
                    snapshot.window.object.path,
                    snapshot.generation
                );
                Ok((
                    screenshot_unavailable(&snapshot, &error.to_string()),
                    error.post_status(),
                ))
            }
            Err(_) => {
                eprintln!(
                    "computer-use-mcp: screenshot capture timed out for pid={} generation={}",
                    snapshot.app.pid, snapshot.generation
                );
                Ok((
                    screenshot_unavailable(&snapshot, "screenshot capture timed out"),
                    PostStatus::Timeout,
                ))
            }
        }
    }

    async fn revalidate_screenshot_target(&self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        if let Some(target) = snapshot.target_ref.as_ref() {
            self.refresh_window_catalog().await?;
            let _ = self.target_entry(target)?;
        }
        if !snapshot.requires_atspi_revalidation {
            return Ok(());
        }
        let apps = self.discover().await?;
        let matching_apps = apps
            .iter()
            .filter(|app| app.pid == snapshot.app.pid && app.object == snapshot.app.object)
            .collect::<Vec<_>>();
        let [app] = matching_apps.as_slice() else {
            return Err(operational_error(
                "application PID or identity changed after screenshot frame acquisition",
            ));
        };
        let matching_windows = app
            .windows
            .iter()
            .filter(|window| window.object == snapshot.window.object)
            .collect::<Vec<_>>();
        let [_window] = matching_windows.as_slice() else {
            return Err(operational_error(
                "window identity changed after screenshot frame acquisition",
            ));
        };
        Ok(())
    }

    fn cache_screenshot(
        &self,
        snapshot: &Snapshot,
        mapping: ScreenshotMapping,
    ) -> Result<(), RuntimeError> {
        let mut cache = self.lock_cache()?;
        let position = cache.position(snapshot).map_err(|error| {
            eprintln!("computer-use-mcp: refusing stale screenshot cache write: {error}");
            error
        })?;
        cache.observations[position].screenshot_mapping = Some(mapping);
        Ok(())
    }

    pub fn screenshot_mapping(
        &self,
        observation_id: &str,
    ) -> Result<Option<ScreenshotMapping>, RuntimeError> {
        let cache = self.lock_cache()?;
        Ok(cache
            .observations
            .iter()
            .find(|cached| observation_id_for_snapshot(&cached.snapshot) == observation_id)
            .and_then(|cached| cached.screenshot_mapping.clone()))
    }

    fn required_cached(&self, observation_id: &str) -> Result<Arc<Snapshot>, RuntimeError> {
        let cache = self.lock_cache()?;
        Ok(Arc::clone(&cache.required(observation_id)?.snapshot))
    }

    fn required_screenshot(
        &self,
        observation_id: &str,
    ) -> Result<(Arc<Snapshot>, ScreenshotMapping), RuntimeError> {
        let cache = self.lock_cache()?;
        let current = cache.required(observation_id)?;
        let mapping = current.screenshot_mapping.clone().ok_or_else(|| {
            state_required_error(
                "the observation has no usable screenshot; call observe and require screenshot.ready=true",
            )
        })?;
        Ok((Arc::clone(&current.snapshot), mapping))
    }

    fn lock_cache(&self) -> Result<std::sync::MutexGuard<'_, Cache>, RuntimeError> {
        self.cache.lock().map_err(|_| {
            eprintln!("computer-use-mcp: state cache mutex poisoned");
            operational_error("state cache invariant failed")
        })
    }

    #[cfg(test)]
    async fn snapshot_text(
        &self,
        _fixture_name: String,
        view: Option<ObserveView>,
        max_nodes: Option<usize>,
        max_depth: Option<usize>,
    ) -> Result<String, RuntimeError> {
        let _mutation = self.mutation.lock().await;
        let scope = match view.unwrap_or(ObserveView::Both) {
            ObserveView::Screenshot => AccessibilityScope::Interactive,
            ObserveView::Accessibility => AccessibilityScope::Full,
            ObserveView::Both => AccessibilityScope::Full,
        };
        let apps = self.discover().await?;
        let [app] = apps.as_slice() else {
            return Err(operational_error(
                "test fixture must expose exactly one AT-SPI application",
            ));
        };
        let [window] = app.windows.as_slice() else {
            return Err(operational_error(
                "test fixture must expose exactly one AT-SPI window",
            ));
        };
        let binding = AtspiBinding {
            app: app.clone(),
            window: window.clone(),
        };
        let snapshot = self
            .collect_snapshot(
                &binding,
                scope,
                None,
                SnapshotLimits {
                    text: self.config.default_text_limit,
                    nodes: max_nodes.unwrap_or(self.config.default_max_nodes),
                    depth: max_depth.unwrap_or(self.config.default_max_depth),
                },
            )
            .await?;
        self.refresh_window_catalog().await?;
        let mut snapshot = snapshot;
        snapshot.screenshot_requested = false;
        snapshot.target_ref = Some(TargetRef {
            app_instance_id: "app-0000000000000000".into(),
            window_instance_id: "win-0000000000000001".into(),
        });
        let snapshot = self.commit_snapshot(snapshot)?;
        Ok(observation_output(
            &snapshot,
            false,
            Some("screenshot was not requested"),
            None,
            None,
            None,
        )
        .text)
    }

    #[cfg(test)]
    async fn element_action_for_test(
        &self,
        observation_id: &str,
        index: &str,
        action: ElementAction,
    ) -> Result<Arc<Snapshot>, RuntimeError> {
        self.element_action(observation_id, index, action, &ActionProgress::default())
            .await
    }
}

impl<A: AccessibilityAdapter, S: ScreenshotProvider> DesktopRuntime for SemanticRuntime<A, S> {
    fn start(&self) {
        self.start_desktop_session();
    }

    async fn wait_for_desktop_session(&self) {
        let _ = self.desktop_session().await;
    }

    fn execute(
        &self,
        call: ToolCall,
        progress: Option<Arc<ActionProgress>>,
    ) -> impl Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_ {
        self.execute_call_with_progress(call, progress)
    }

    async fn cleanup(&self, progress: Option<Arc<ActionProgress>>) -> Result<(), RuntimeError> {
        let _mutation = self.mutation.lock().await;
        self.lock_cache()?.clear_screenshot_mappings();
        let launch_result = crate::desktop_launcher::cancel_and_join(
            Arc::clone(&self.launch_in_progress),
            Arc::clone(&self.launch_tasks),
            Duration::from_secs(2),
        )
        .await;
        let result = self
            .screenshots
            .cleanup_input()
            .await
            .map_err(operational_error);
        let cleanup_result = match (launch_result, result) {
            (Ok(()), result) => result,
            (Err(launch), Ok(())) => Err(launch),
            (Err(launch), Err(cleanup)) => Err(operational_error(format!(
                "desktop launch cleanup failed: {launch}; input cleanup also failed: {cleanup}"
            ))),
        };
        if let Some(progress) = progress {
            if cleanup_result.is_ok() {
                progress.mark_cleanup_completed();
            } else {
                progress.mark_cleanup_failed();
            }
        }
        cleanup_result
    }

    async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.stop_desktop_session().await;
        let _mutation = self.mutation.lock().await;
        let launch_result = crate::desktop_launcher::cancel_and_join(
            Arc::clone(&self.launch_in_progress),
            Arc::clone(&self.launch_tasks),
            Duration::from_secs(2),
        )
        .await;
        let screenshot_result = self
            .screenshots
            .shutdown_input()
            .await
            .map_err(operational_error);
        let wayland_result = self.wayland.shutdown().await.map_err(|error| {
            operational_error(format!("Wayland catalog shutdown failed: {error}"))
        });
        match (launch_result, screenshot_result.and(wayland_result)) {
            (Ok(()), result) => result,
            (Err(launch), Ok(())) => Err(launch),
            (Err(launch), Err(other)) => Err(operational_error(format!(
                "desktop launch shutdown failed: {launch}; other shutdown failed: {other}"
            ))),
        }
    }
}

#[derive(Debug)]
struct Traversal {
    elements: Vec<ElementSnapshot>,
    node_limit_reached: bool,
    depth_limit_reached: bool,
}

fn window_is_viable(window: &WindowInfo) -> bool {
    !window.states.contains("defunct") && !window.states.contains("stale")
}

fn normalize_frame(node: &NodeInfo) -> Option<Rect> {
    node.window_frame.filter(|frame| frame.is_valid())
}

fn is_hidden_document(node: &NodeInfo) -> bool {
    node.role.to_ascii_lowercase().contains("document")
        && !node.states.contains("showing")
        && !node.states.contains("active")
        && !node.states.contains("focused")
}

fn relocate<'a>(
    old: &ElementSnapshot,
    current: &'a [ElementSnapshot],
) -> Result<&'a ElementSnapshot, RuntimeError> {
    let mut matches = current.iter().filter(|candidate| {
        candidate.node.object == old.node.object && same_role_name(candidate, old)
    });
    let Some(candidate) = matches.next() else {
        return Err(operational_error(
            "element object identity changed; call observe again",
        ));
    };
    if matches.next().is_some() {
        return Err(operational_error("element object identity is ambiguous"));
    }
    usable(candidate)
}

fn same_role_name(candidate: &ElementSnapshot, old: &ElementSnapshot) -> bool {
    candidate.node.role == old.node.role && candidate.node.name == old.node.name
}

fn usable(element: &ElementSnapshot) -> Result<&ElementSnapshot, RuntimeError> {
    if element.node.is_defunct() {
        return Err(operational_error("relocated element is defunct"));
    }
    Ok(element)
}

fn semantic_action(
    action: ElementAction,
    element: &ElementSnapshot,
) -> Result<SemanticAction, RuntimeError> {
    match action {
        ElementAction::Invoke => {
            let actions = element
                .node
                .capabilities
                .inspected_actions()
                .map_err(|()| {
                    operational_error(
                        "AT-SPI action capability inspection failed for the target element",
                    )
                })?;
            match primary_action_index(actions) {
                Some(index) => i32::try_from(index)
                    .map(SemanticAction::InvokeAction)
                    .map_err(|_| operational_error("AT-SPI action index overflow")),
                None => Err(operational_error(
                    "element exposes no recognized primary AT-SPI action",
                )),
            }
        }
        ElementAction::Named(requested) => {
            let actions = element
                .node
                .capabilities
                .inspected_actions()
                .map_err(|()| {
                    operational_error(
                        "AT-SPI action capability inspection failed for the target element",
                    )
                })?;
            let matches: Vec<_> = named_actions(actions)
                .filter(|(_, action)| action.name == requested)
                .collect();
            match matches.as_slice() {
                [(index, _)] => i32::try_from(*index)
                    .map(SemanticAction::InvokeAction)
                    .map_err(|_| operational_error("AT-SPI action index overflow")),
                [] => Err(operational_error(format!(
                    "named action {requested:?} is not exposed by the element"
                ))),
                _ => Err(operational_error(format!(
                    "named action {requested:?} matches more than one action"
                ))),
            }
        }
        ElementAction::Focus => {
            if !element.node.supports_focus() {
                return Err(capability_error(
                    "element does not expose AT-SPI Component with the focusable state",
                ));
            }
            Ok(SemanticAction::GrabFocus)
        }
        ElementAction::SetValue(value) => {
            if !element.node.capabilities.interfaces_inspected() {
                return Err(operational_error(
                    "AT-SPI interface inspection failed for the target element",
                ));
            }
            match element.node.capabilities.set_value_kind() {
                Some(SetValueKind::Text) => Ok(SemanticAction::ReplaceText(value)),
                Some(SetValueKind::Number) => {
                    let numeric = value.parse::<f64>().map_err(|_| {
                        operational_error("AT-SPI Value requires a finite numeric value")
                    })?;
                    if !numeric.is_finite() {
                        return Err(operational_error(
                            "AT-SPI Value requires a finite numeric value",
                        ));
                    }
                    Ok(SemanticAction::SetNumericValue(numeric))
                }
                None => Err(operational_error(
                    "element supports neither EditableText nor Value",
                )),
            }
        }
    }
}

fn primary_action_index(actions: &[ActionInfo]) -> Option<usize> {
    const PREFERRED: [&str; 8] = [
        "click", "press", "activate", "invoke", "select", "toggle", "open", "default",
    ];
    PREFERRED
        .iter()
        .find_map(|preferred| {
            actions
                .iter()
                .position(|action| action.name.eq_ignore_ascii_case(preferred))
        })
        .or_else(|| {
            (actions.len() == 1
                && actions.iter().all(|action| {
                    action.name.trim().is_empty() && action.description.trim().is_empty()
                }))
            .then_some(0)
        })
}

fn named_actions(actions: &[ActionInfo]) -> impl Iterator<Item = (usize, &ActionInfo)> {
    actions
        .iter()
        .enumerate()
        .filter(|(_, action)| !action.name.trim().is_empty())
}

pub fn format_snapshot(snapshot: &Snapshot) -> String {
    format_snapshot_with_budget(snapshot, MAX_MODEL_TEXT_BYTES).text
}

fn format_snapshot_with_budget(snapshot: &Snapshot, max_bytes: usize) -> TextProjection {
    let presented = presented_element_indices(snapshot);
    let first = build_snapshot_text(snapshot, &presented, max_bytes);
    let Some(focused) = presented.iter().copied().find(|index| {
        snapshot
            .elements
            .get(*index)
            .is_some_and(|element| element.node.states.contains("focused"))
    }) else {
        return first;
    };
    if !first.truncation.truncated
        || first
            .text
            .contains(&element_id_for_snapshot(snapshot, focused))
    {
        return first;
    }

    // Preserve the root and focused element when a response budget forces a
    // choice.  The normal order remains unchanged whenever the complete
    // observation fits.
    let mut prioritized = Vec::with_capacity(presented.len());
    if let Some(root) = presented.first().copied() {
        prioritized.push(root);
    }
    prioritized.push(focused);
    let rest = presented
        .iter()
        .copied()
        .filter(|index| !prioritized.contains(index))
        .collect::<Vec<_>>();
    prioritized.extend(rest);
    build_snapshot_text(snapshot, &prioritized, max_bytes)
}

fn build_snapshot_text(
    snapshot: &Snapshot,
    element_indexes: &[usize],
    max_bytes: usize,
) -> TextProjection {
    let (app_instance_id, window_instance_id) = snapshot.target_ref.as_ref().map_or_else(
        || {
            (
                format!("app-{:016x}", u64::from(snapshot.app.pid)),
                format!("win-{:016x}", snapshot.generation),
            )
        },
        |target| {
            (
                target.app_instance_id.clone(),
                target.window_instance_id.clone(),
            )
        },
    );
    let mut output = format!(
        "Observation ID: {} target=app_instance_id={} window_instance_id={}\naccessibility_bounds=not_convertible\n",
        observation_id_for_snapshot(snapshot),
        app_instance_id,
        window_instance_id,
    );
    let marker_reserve = ResponseTruncation::default().text_marker().len() + 1;
    let body_budget = max_bytes.saturating_sub(marker_reserve);
    let mut focused = None;
    let mut selected = None;
    let mut included = 0;
    let mut fields_truncated = false;
    for index in element_indexes {
        let Some(element) = snapshot.elements.get(*index) else {
            eprintln!("computer-use-mcp: snapshot element index disappeared during formatting");
            continue;
        };
        let remaining = body_budget.saturating_sub(output.len());
        let Some((line, line_truncated)) =
            fit_element_text_line(snapshot, *index, element, remaining)
        else {
            break;
        };
        output.push_str(&line);
        included += 1;
        fields_truncated |= line_truncated;
        if element.node.states.contains("focused") {
            focused = Some(element_id_for_snapshot(snapshot, *index));
        }
        if selected.is_none() {
            selected = element
                .node
                .selected_text
                .as_ref()
                .filter(|text| !text.is_empty())
                .map(|text| truncate(text, snapshot.limits.text.min(MAX_MODEL_FIELD_CHARS)));
        }
    }

    let mut append_optional = |line: String| {
        if output.len().saturating_add(line.len()) <= body_budget {
            output.push_str(&line);
        } else {
            fields_truncated = true;
        }
    };
    if let Some(element_id) = focused {
        append_optional(format!("Focused element: {element_id}\n"));
    }
    if let Some(text) = selected {
        append_optional(format!("Selected text: \"{}\"\n", escape(&text)));
    }
    if snapshot.node_limit_reached {
        append_optional("Warning: accessibility tree node limit reached.\n".into());
    }
    if snapshot.depth_limit_reached {
        append_optional("Warning: accessibility tree depth limit reached.\n".into());
    }

    let elements_omitted = element_indexes.len().saturating_sub(included);
    let truncation = ResponseTruncation {
        truncated: elements_omitted > 0 || fields_truncated,
        fields_truncated,
        elements_included: included,
        elements_omitted,
    };
    if truncation.truncated {
        let marker = format!("{}\n", truncation.text_marker());
        while output.len().saturating_add(marker.len()) > max_bytes {
            let Some(last_newline) = output[..output.len().saturating_sub(1)].rfind('\n') else {
                break;
            };
            output.truncate(last_newline + 1);
        }
        output.push_str(&marker);
    }
    TextProjection {
        text: output,
        truncation,
    }
}

fn fit_element_text_line(
    snapshot: &Snapshot,
    index: usize,
    element: &ElementSnapshot,
    remaining: usize,
) -> Option<(String, bool)> {
    let upper = MAX_MODEL_FIELD_CHARS;
    let mut low = 0;
    let mut high = upper;
    let mut best = None;
    while low <= high {
        let field_limit = low + (high - low) / 2;
        let (line, field_truncated) = element_text_line(snapshot, index, element, field_limit);
        if line.len() <= remaining {
            best = Some((line, field_truncated || field_limit < upper));
            low = field_limit.saturating_add(1);
        } else if field_limit == 0 {
            break;
        } else {
            high = field_limit - 1;
        }
    }
    best
}

fn element_text_line(
    snapshot: &Snapshot,
    index: usize,
    element: &ElementSnapshot,
    field_limit: usize,
) -> (String, bool) {
    let mut truncated_fields = false;
    let field = |value: &str, truncated_fields: &mut bool| {
        let was_truncated = value.chars().count() > field_limit;
        let value = truncate(value, field_limit);
        *truncated_fields |= was_truncated;
        escape(&value)
    };
    let value_field = |value: &str| {
        let value = truncate(value, snapshot.limits.text);
        let truncated = value.chars().count() > field_limit;
        (escape(&truncate(&value, field_limit)), truncated)
    };
    let indent = "\t".repeat(element.depth + 1);
    let role = field(&element.node.role, &mut truncated_fields);
    let name = field(&element.node.name, &mut truncated_fields);
    let mut output = format!(
        "{indent}{}: {role} name=\"{name}\"",
        element_id_for_snapshot(snapshot, index),
    );
    if let Some(value) = element.node.value.as_ref().or(element.node.text.as_ref()) {
        let (value, value_truncated) = value_field(value);
        truncated_fields |= value_truncated;
        output.push_str(&format!(" value=\"{value}\""));
    }
    if element.node.text_truncated == Some(true) {
        output.push_str(" text_truncated=true");
    }
    let capabilities = text_capabilities(element);
    if !capabilities.is_empty() {
        output.push_str(" capabilities=[");
        output.push_str(&capabilities.join(", "));
        output.push(']');
    }
    let actions = named_actions(element.node.capabilities.actions())
        .take(MAX_MODEL_ACTIONS)
        .map(|(_, action)| {
            let name = field(&action.name, &mut truncated_fields);
            let description = field(&action.description, &mut truncated_fields);
            if description.is_empty() {
                format!("{{name=\"{name}\"}}")
            } else {
                format!("{{name=\"{name}\", description=\"{description}\"}}")
            }
        })
        .collect::<Vec<_>>();
    if !actions.is_empty() {
        output.push_str(" named_actions=[");
        output.push_str(&actions.join(", "));
        output.push(']');
    }
    if element.node.capabilities.actions().len() > MAX_MODEL_ACTIONS {
        output.push_str(" named_actions_truncated=true");
        truncated_fields = true;
    }
    output.push('\n');
    (output, truncated_fields)
}

fn screenshot_unavailable(snapshot: &Snapshot, reason: &str) -> ToolOutput {
    observation_output(snapshot, false, Some(reason), None, None, None)
}

fn installed_app_page_generation(apps: &[crate::desktop_launcher::InstalledApp]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for app in apps {
        for value in [&app.desktop_id, &app.name] {
            for byte in value.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= if app.shown { 1 } else { 0 };
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn page_scope_name(scope: DesktopScope) -> &'static str {
    match scope {
        DesktopScope::Windows => "windows",
        DesktopScope::Applications => "applications",
    }
}

fn page_cursor(scope: DesktopScope, generation: u64, offset: usize) -> String {
    format!(
        "cur-{}-{generation:016x}-{offset:016x}",
        page_scope_name(scope)
    )
}

fn page_start(
    cursor: Option<&str>,
    scope: DesktopScope,
    generation: u64,
    total: usize,
) -> Result<usize, RuntimeError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let parts = cursor.split('-').collect::<Vec<_>>();
    if parts.len() != 4
        || parts[0] != "cur"
        || parts[1] != page_scope_name(scope)
        || parts[2].len() != 16
        || parts[3].len() != 16
        || !parts[2]
            .bytes()
            .chain(parts[3].bytes())
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RuntimeError::invalid_arguments("cursor is malformed"));
    }
    let cursor_generation = u64::from_str_radix(parts[2], 16)
        .map_err(|_| RuntimeError::invalid_arguments("cursor is malformed"))?;
    let offset = usize::from_str_radix(parts[3], 16)
        .map_err(|_| RuntimeError::invalid_arguments("cursor is malformed"))?;
    if cursor_generation != generation || offset > total {
        return Err(stale_catalog_error(
            "cursor is stale; request the first page again",
        ));
    }
    Ok(offset)
}

fn compact_backend_status(status: &BackendStatus) -> String {
    compact_capability(&status.capability())
}

fn compact_capability(status: &CapabilityState) -> String {
    status.reason().map_or_else(
        || status.status().to_owned(),
        |reason| format!("{}({})", status.status(), truncate(&escape(reason), 96)),
    )
}

fn compact_window_entry(entry: &WindowEntry) -> String {
    format!(
        "{} — app_instance_id={} window_instance_id={} pid={} source={} capabilities=screenshot:{} accessibility:{} activation:{}",
        truncate(&escape(&entry.title), 160),
        entry.target.app_instance_id,
        entry.target.window_instance_id,
        entry
            .pid
            .map_or_else(|| "null".to_owned(), |pid| pid.to_string()),
        entry.source.as_str(),
        compact_capability(&entry.capabilities.screenshot),
        compact_capability(&entry.capabilities.accessibility),
        compact_capability(&entry.capabilities.activate),
    )
}

fn bounded_list_text(header: &str, entries: &[String], next_cursor: Option<&str>) -> String {
    let prefix = if header.is_empty() {
        String::new()
    } else {
        format!("{header}\n")
    };
    let next_line = next_cursor.map(|cursor| format!("Next cursor: {cursor}\n"));
    let mut output = prefix.clone();
    for entry in entries {
        let separator = usize::from(!output.is_empty() && !output.ends_with('\n'));
        if output
            .len()
            .saturating_add(separator)
            .saturating_add(entry.len())
            .saturating_add(next_line.as_ref().map_or(0, String::len))
            <= MAX_MODEL_TEXT_BYTES
        {
            if separator != 0 {
                output.push('\n');
            }
            output.push_str(entry);
        } else {
            break;
        }
    }
    if output
        .len()
        .saturating_add(next_line.as_ref().map_or(0, String::len))
        <= MAX_MODEL_TEXT_BYTES
        && output
            .lines()
            .count()
            .saturating_sub(usize::from(!header.is_empty()))
            == entries.len()
    {
        if let Some(next_line) = next_line {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&next_line);
        }
        return output;
    }

    let marker_budget = 128;
    let next_len = next_line.as_ref().map_or(0, String::len);
    let mut selected: Vec<&String> = Vec::new();
    for entry in entries {
        let separator = usize::from(!prefix.is_empty() || !selected.is_empty());
        let candidate_len = prefix
            .len()
            .saturating_add(selected.iter().map(|entry| entry.len()).sum::<usize>())
            .saturating_add(selected.len())
            .saturating_add(separator)
            .saturating_add(entry.len())
            .saturating_add(marker_budget)
            .saturating_add(next_len);
        if candidate_len > MAX_MODEL_TEXT_BYTES {
            break;
        }
        selected.push(entry);
    }
    let mut omitted = entries.len().saturating_sub(selected.len());
    let mut marker = format!(
        "List text truncated: reason=response_byte_budget entries_included={} entries_omitted={}\n",
        selected.len(),
        omitted
    );
    loop {
        let mut bounded = prefix.clone();
        for entry in &selected {
            if !bounded.is_empty() && !bounded.ends_with('\n') {
                bounded.push('\n');
            }
            bounded.push_str(entry);
            if !bounded.ends_with('\n') {
                bounded.push('\n');
            }
        }
        bounded.push_str(&marker);
        if let Some(next_line) = next_line.as_ref() {
            bounded.push_str(next_line);
        }
        if bounded.len() <= MAX_MODEL_TEXT_BYTES || selected.is_empty() {
            return bounded;
        }
        selected.pop();
        omitted = entries.len().saturating_sub(selected.len());
        marker = format!(
            "List text truncated: reason=response_byte_budget entries_included={} entries_omitted={}\n",
            selected.len(),
            omitted
        );
    }
}

fn text_capabilities(element: &ElementSnapshot) -> Vec<String> {
    let mut capabilities = Vec::new();
    if element
        .node
        .capabilities
        .inspected_actions()
        .is_ok_and(|actions| primary_action_index(actions).is_some())
    {
        capabilities.push("invoke".into());
    }
    if element.node.supports_focus() {
        capabilities.push("focus".into());
    }
    match element.node.capabilities.set_value_kind() {
        Some(SetValueKind::Text) => capabilities.push("set_value:text".into()),
        Some(SetValueKind::Number) => capabilities.push("set_value:number".into()),
        None => {}
    }
    capabilities
}

fn element_id_for_snapshot(snapshot: &Snapshot, index: usize) -> String {
    snapshot
        .element_ids
        .get(index)
        .cloned()
        .unwrap_or_else(|| format!("e-{index:016x}"))
}

fn element_capabilities_for_snapshot_with_limit(
    snapshot: &Snapshot,
    index: usize,
    element: &ElementSnapshot,
    field_limit: usize,
) -> (serde_json::Value, bool) {
    let inspection_complete = element.node.capabilities.inspection_complete();
    let set_value = match element.node.capabilities.set_value_kind() {
        Some(SetValueKind::Text) => Some("text"),
        Some(SetValueKind::Number) => Some("number"),
        None => None,
    };
    let actions = element.node.capabilities.actions();
    let element_id = element_id_for_snapshot(snapshot, index);
    let (role, role_truncated) = response_field(&element.node.role, field_limit);
    let (name, name_truncated) = response_field(&element.node.name, field_limit);
    let value_limit = field_limit.min(snapshot.limits.text);
    let (value, value_truncated) = element
        .node
        .value
        .as_deref()
        .map_or((None, false), |value| {
            let (value, truncated) = response_field(value, value_limit);
            (Some(value), truncated)
        });
    let (text, text_truncated) = element.node.text.as_deref().map_or((None, false), |text| {
        let (text, truncated) = response_field(text, value_limit);
        (Some(text), truncated)
    });
    let (states, states_truncated) = bounded_response_strings(
        element.node.states.iter().map(String::as_str),
        field_limit,
        MAX_MODEL_STATES,
    );
    let (named_actions, actions_truncated) = bounded_named_actions(actions, field_limit);
    let fields_truncated = role_truncated
        || name_truncated
        || value_truncated
        || text_truncated
        || states_truncated
        || actions_truncated;
    (
        json!({
            "element_id": element_id,
            "depth": element.depth,
            "role": role,
            "name": name,
            "value": value,
            "text": text,
            "text_truncated": element.node.text_truncated,
            "response_text_truncated": fields_truncated,
            "states": states,
            "states_truncated": states_truncated,
            "bounds": null,
            "capabilities": {
                "invoke": inspection_complete && primary_action_index(actions).is_some(),
                "focus": element.node.supports_focus(),
                "named_actions": named_actions,
                "named_actions_truncated": actions_truncated,
                "set_value": set_value
            }
        }),
        fields_truncated,
    )
}

fn response_field(value: &str, limit: usize) -> (String, bool) {
    if limit == 0 {
        return (String::new(), !value.is_empty());
    }
    let truncated = value.chars().count() > limit;
    (truncate(value, limit), truncated)
}

fn bounded_response_strings<'a>(
    values: impl IntoIterator<Item = &'a str>,
    field_limit: usize,
    max_items: usize,
) -> (Vec<String>, bool) {
    let mut truncated = false;
    let values = values
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| {
            if index >= max_items {
                truncated = true;
                return None;
            }
            let (value, value_truncated) = response_field(value, field_limit);
            truncated |= value_truncated;
            Some(value)
        })
        .collect();
    (values, truncated)
}

fn bounded_named_actions(actions: &[ActionInfo], field_limit: usize) -> (Vec<Value>, bool) {
    let mut truncated = actions.len() > MAX_MODEL_ACTIONS;
    let actions = actions
        .iter()
        .take(MAX_MODEL_ACTIONS)
        .map(|action| {
            let (name, name_truncated) = response_field(&action.name, field_limit);
            let (description, description_truncated) =
                response_field(&action.description, field_limit);
            truncated |= name_truncated || description_truncated;
            json!({"name": name, "description": description})
        })
        .collect();
    (actions, truncated)
}

#[cfg(test)]
fn element_capabilities(index: usize, element: &ElementSnapshot) -> serde_json::Value {
    let complete = element.node.capabilities.inspection_complete();
    let actions = element.node.capabilities.actions();
    json!({
        "element_id": format!("e-{index:016x}"),
        "depth": element.depth,
        "role": element.node.role,
        "name": element.node.name,
        "value": element.node.value,
        "text": element.node.text,
        "text_truncated": element.node.text_truncated,
        "states": element.node.states.iter().collect::<Vec<_>>(),
        "inspection_complete": complete,
        "invoke": complete && primary_action_index(actions).is_some(),
        "focus": element.node.supports_focus(),
        "named_actions": named_actions(actions)
            .map(|(_, action)| json!({"name": action.name, "description": action.description}))
            .collect::<Vec<_>>(),
        "set_value": match element.node.capabilities.set_value_kind() {
            Some(SetValueKind::Text) => json!("text"),
            Some(SetValueKind::Number) => json!("number"),
            None => Value::Null,
        },
    })
}

fn observation_output(
    snapshot: &Snapshot,
    screenshot_ready: bool,
    screenshot_reason: Option<&str>,
    dimensions: Option<(u32, u32)>,
    png_base64: Option<String>,
    source: Option<&crate::capture::FrameMetadata>,
) -> ToolOutput {
    let structured = build_observation_structured(
        snapshot,
        screenshot_ready,
        screenshot_reason,
        dimensions,
        source,
        MAX_MODEL_STRUCTURED_BYTES,
    );
    let frame_id = source.map(crate::capture::frame_id);
    let png_text = if screenshot_ready {
        match (frame_id.as_deref(), dimensions) {
            (Some(frame_id), Some((width, height))) => {
                format!("PNG frame_id={frame_id} bounds=0<=x<{width},0<=y<{height}")
            }
            (Some(frame_id), None) => format!("PNG frame_id={frame_id} bounds=unavailable"),
            _ => "PNG unavailable: frame ID unavailable".to_owned(),
        }
    } else {
        format!(
            "PNG unavailable: {}",
            truncate(
                &escape(screenshot_reason.unwrap_or("screenshot not requested")),
                MAX_MODEL_FIELD_CHARS,
            )
        )
    };
    let snapshot_budget = MAX_MODEL_TEXT_BYTES.saturating_sub(png_text.len() + 1);
    let snapshot_text = format_snapshot_with_budget(snapshot, snapshot_budget).text;
    let text = format!("{png_text}\n{snapshot_text}");
    let mut output = ToolOutput::text(text).with_structured_content(structured);
    if let Some(png) = png_base64 {
        output = output.with_png_base64(png);
    }
    output
}

fn build_observation_structured(
    snapshot: &Snapshot,
    screenshot_ready: bool,
    screenshot_reason: Option<&str>,
    dimensions: Option<(u32, u32)>,
    source: Option<&crate::capture::FrameMetadata>,
    max_bytes: usize,
) -> Value {
    let (width, height) =
        dimensions.map_or((None, None), |(width, height)| (Some(width), Some(height)));
    let screenshot_scope = if screenshot_ready {
        "monitor"
    } else {
        "unavailable"
    };
    let frame_id = source.map(crate::capture::frame_id);
    let screenshot = json!({
        "ready": screenshot_ready,
        "reason": screenshot_reason,
        "scope": screenshot_scope,
        "width": width,
        "height": height,
        "frame_id": frame_id,
        "coordinate_space": "screenshot_png_pixels",
        "metadata": source.map(frame_metadata).unwrap_or_else(frame_metadata_unavailable)
    });
    let target = snapshot.target_ref.as_ref().map_or_else(
        || {
            json!({
                "app_instance_id": format!("app-{:016x}", u64::from(snapshot.app.pid)),
                "window_instance_id": format!("win-{:016x}", snapshot.generation)
            })
        },
        TargetRef::as_json,
    );
    let (accessibility_ready, accessibility_reason) = if snapshot.accessibility_ready {
        (true, snapshot.accessibility_reason.as_deref())
    } else {
        (
            false,
            snapshot
                .accessibility_reason
                .as_deref()
                .or(screenshot_reason),
        )
    };
    let accessibility_requested = snapshot.accessibility_ready
        || snapshot.accessibility_reason.as_deref() != Some("accessibility was not requested");
    let view = match (snapshot.screenshot_requested, accessibility_requested) {
        (true, true) => "both",
        (true, false) => "screenshot",
        (false, _) => "accessibility",
    };
    let accessibility_scope = snapshot.view.as_str();
    let text_limit = (snapshot.limits.text != usize::MAX).then_some(snapshot.limits.text);
    let element_indexes = presented_element_indices(snapshot);
    let build = |elements: &[Value], truncation: ResponseTruncation| {
        let truncation_json = truncation.json();
        json!({
            "observation_id": observation_id_for_snapshot(snapshot),
            "target": target,
            "view": view,
            "truncated": truncation.truncated,
            "truncation_reason": truncation.truncated.then_some("response_byte_budget"),
            "truncation": truncation_json,
            "accessibility": {
                "ready": accessibility_ready,
                "reason": accessibility_reason,
                "scope": accessibility_scope,
                "query": snapshot.element_query,
                "truncated": truncation.truncated,
                "truncation_reason": truncation.truncated.then_some("response_byte_budget"),
                "limits": {
                    "text_limit": text_limit,
                    "max_nodes": snapshot.limits.nodes,
                    "max_depth": snapshot.limits.depth
                }
            },
            "screenshot": screenshot,
            "coordinate_spaces": {
                "png": "screenshot_png_pixels",
                "source_logical_size": source.map(|source| json!([source.size.0, source.size.1])),
                "transform": source.map(|source| format!("{:?}", source.transform)).unwrap_or_else(|| "unavailable".into()),
                "transform_authority": source.map_or("unavailable", |_| "pipewire"),
                "accessibility_bounds": "not_convertible"
            },
            "elements": elements,
            "png": Value::Null
        })
    };
    let (structured, truncation) =
        fit_structured_elements(snapshot, &element_indexes, max_bytes, build);
    let focused = element_indexes.iter().copied().find(|index| {
        snapshot
            .elements
            .get(*index)
            .is_some_and(|element| element.node.states.contains("focused"))
    });
    if truncation.truncated
        && focused.is_some_and(|index| {
            let element_id = element_id_for_snapshot(snapshot, index);
            !structured["elements"].as_array().is_some_and(|elements| {
                elements
                    .iter()
                    .any(|element| element["element_id"] == element_id)
            })
        })
    {
        let focused = focused.expect("focused element was checked");
        let mut prioritized = Vec::with_capacity(element_indexes.len());
        if let Some(root) = element_indexes.first().copied() {
            prioritized.push(root);
        }
        prioritized.push(focused);
        let rest = element_indexes
            .iter()
            .copied()
            .filter(|index| !prioritized.contains(index))
            .collect::<Vec<_>>();
        prioritized.extend(rest);
        return fit_structured_elements(snapshot, &prioritized, max_bytes, build).0;
    }
    structured
}

fn fit_structured_elements<F>(
    snapshot: &Snapshot,
    element_indexes: &[usize],
    max_bytes: usize,
    build: F,
) -> (Value, ResponseTruncation)
where
    F: Fn(&[Value], ResponseTruncation) -> Value,
{
    let mut included = Vec::new();
    let mut fields_truncated = false;
    let field_limit = MAX_MODEL_FIELD_CHARS;
    for index in element_indexes {
        let Some(element) = snapshot.elements.get(*index) else {
            eprintln!(
                "computer-use-mcp: snapshot element index disappeared during structured formatting"
            );
            continue;
        };
        let mut low = 0;
        let mut high = field_limit;
        let mut best = None;
        while low <= high {
            let candidate_limit = low + (high - low) / 2;
            let (element_value, element_truncated) = element_capabilities_for_snapshot_with_limit(
                snapshot,
                *index,
                element,
                candidate_limit,
            );
            let mut candidate_elements = included.clone();
            candidate_elements.push(element_value.clone());
            let provisional = ResponseTruncation {
                truncated: true,
                fields_truncated: fields_truncated || element_truncated,
                elements_included: candidate_elements.len(),
                elements_omitted: element_indexes
                    .len()
                    .saturating_sub(candidate_elements.len()),
            };
            let candidate = build(&candidate_elements, provisional);
            if json_size(&candidate) <= max_bytes {
                best = Some((
                    element_value,
                    element_truncated || candidate_limit < field_limit,
                ));
                low = candidate_limit.saturating_add(1);
            } else if candidate_limit == 0 {
                break;
            } else {
                high = candidate_limit - 1;
            }
        }
        let Some((element, element_truncated)) = best else {
            break;
        };
        fields_truncated |= element_truncated;
        included.push(element);
    }
    let truncation = ResponseTruncation {
        truncated: included.len() < element_indexes.len() || fields_truncated,
        fields_truncated,
        elements_included: included.len(),
        elements_omitted: element_indexes.len().saturating_sub(included.len()),
    };
    let value = build(&included, truncation);
    if json_size(&value) > max_bytes {
        eprintln!(
            "computer-use-mcp: structured observation budget invariant failed after element trimming"
        );
    }
    (value, truncation)
}

fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("structured output values must be serializable")
        .len()
}

fn presented_element_indices(snapshot: &Snapshot) -> Vec<usize> {
    presented_elements(snapshot)
        .map(|(index, _)| index)
        .collect()
}

fn observation_id_for_snapshot(snapshot: &Snapshot) -> String {
    format!("obs-{:016x}", snapshot.generation)
}

fn frame_metadata(source: &crate::capture::FrameMetadata) -> serde_json::Value {
    json!({
        "frame_id": crate::capture::frame_id(source),
        "source_sequence": source.source_sequence,
        "pts_ns": source.pts_ns,
        "arrival_monotonic_ns": source.arrival_monotonic_ns,
        "format_epoch": source.format_generation,
        "size": [source.size.0, source.size.1],
        "crop": {"x": source.crop.x, "y": source.crop.y, "width": source.crop.width, "height": source.crop.height},
        "transform": format!("{:?}", source.transform),
        "timestamp_authority": source.timestamp_authority.as_str(),
        "stream_health": source.stream_health.as_str(),
        "content_hash": format!("{:016x}", source.content_hash),
        "change_epoch": source.change_epoch,
        "changed_from_previous": source.changed_from_previous,
        "sequence_gap": source.sequence_gap
    })
}

fn frame_metadata_unavailable() -> serde_json::Value {
    json!({
        "frame_id": null,
        "source_sequence": null,
        "pts_ns": null,
        "arrival_monotonic_ns": 0,
        "format_epoch": 0,
        "size": [0, 0],
        "crop": {"x": 0, "y": 0, "width": 0, "height": 0},
        "transform": "unavailable",
        "timestamp_authority": "unavailable",
        "stream_health": "failed",
        "content_hash": null,
        "change_epoch": 0,
        "changed_from_previous": null,
        "sequence_gap": null
    })
}

fn presented_elements(snapshot: &Snapshot) -> impl Iterator<Item = (usize, &ElementSnapshot)> {
    let query = snapshot.element_query.as_deref().map(str::to_lowercase);
    snapshot
        .elements
        .iter()
        .enumerate()
        .filter(move |(_, element)| {
            element_is_presented_with_query(snapshot.view, query.as_deref(), element)
        })
}

fn element_is_presented_with_query(
    view: AccessibilityScope,
    query: Option<&str>,
    element: &ElementSnapshot,
) -> bool {
    (view != AccessibilityScope::Interactive || is_interactive(element))
        && query.is_none_or(|query| element_matches_query(element, query))
}

fn is_interactive(element: &ElementSnapshot) -> bool {
    if !element.node.capabilities.actions().is_empty()
        || element.node.capabilities.set_value_kind().is_some()
    {
        return true;
    }
    const ROLES: [&str; 13] = [
        "button",
        "check box",
        "combo box",
        "entry",
        "link",
        "list item",
        "menu item",
        "page tab",
        "radio button",
        "slider",
        "spin button",
        "switch",
        "toggle button",
    ];
    ROLES
        .iter()
        .any(|role| element.node.role.eq_ignore_ascii_case(role))
        || element.node.states.iter().any(|state| {
            matches!(
                state.as_str(),
                "checkable" | "editable" | "focusable" | "selectable"
            )
        })
}

fn element_matches_query(element: &ElementSnapshot, query: &str) -> bool {
    [
        Some(element.node.role.as_str()),
        Some(element.node.name.as_str()),
        element.node.value.as_deref(),
        element.node.text.as_deref(),
        element.node.selected_text.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(query))
        || element
            .node
            .states
            .iter()
            .any(|state| state.to_lowercase().contains(query))
        || element.node.capabilities.actions().iter().any(|action| {
            action.name.to_lowercase().contains(query)
                || action.description.to_lowercase().contains(query)
        })
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

fn truncate(value: &str, limit: usize) -> String {
    if limit == usize::MAX {
        return value.to_owned();
    }
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn operational_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::not_started("target_unavailable", message)
}

fn catalog_error(error: CatalogError) -> RuntimeError {
    match error {
        CatalogError::StaleTarget(target) => stale_catalog_error(format!(
            "window target is stale: app_instance_id={} window_instance_id={}",
            target.app_instance_id, target.window_instance_id
        )),
        CatalogError::InvalidIdentity(identity) => {
            eprintln!("computer-use-mcp: window catalog rejected backend identity {identity:?}");
            operational_error("window backend returned an invalid explicit identity")
        }
        CatalogError::GenerationExhausted => {
            eprintln!("computer-use-mcp: window catalog generation exhausted");
            operational_error("window catalog generation exhausted")
        }
    }
}

fn backend_activation_error(error: BackendError) -> RuntimeError {
    match error {
        BackendError::Stale(reason) => stale_catalog_error(reason),
        BackendError::Unsupported(reason) | BackendError::Busy(reason) => capability_error(reason),
        BackendError::Unavailable(reason) => RuntimeError::new(
            "backend_unavailable",
            reason,
            ToolOutcome::NotStarted,
            true,
            "Call list_desktop again and retry only if the exact target and capability remain available.",
        ),
        BackendError::Unknown(reason) => RuntimeError::new(
            "activation_unknown",
            reason,
            ToolOutcome::Unknown,
            false,
            "Call list_desktop, observe the exact target, and do not retry activation blindly.",
        ),
        BackendError::Failed(reason) => uncertain_action(operational_error(reason)),
    }
}

fn parse_opaque_counter(value: &str, prefix: &str) -> Result<u64, RuntimeError> {
    let expected = prefix.len() + 1 + 16;
    if value.len() != expected || !value.starts_with(&format!("{prefix}-")) {
        return Err(stale_observation_error(
            "observation ID is malformed or stale",
        ));
    }
    u64::from_str_radix(&value[prefix.len() + 1..], 16)
        .map_err(|_| stale_observation_error("observation ID is malformed or stale"))
}

fn same_snapshot_content(left: &Snapshot, right: &Snapshot) -> bool {
    left.app == right.app
        && left.window == right.window
        && left.view == right.view
        && left.element_query == right.element_query
        && left.elements == right.elements
        && left.node_limit_reached == right.node_limit_reached
        && left.depth_limit_reached == right.depth_limit_reached
        && left.limits == right.limits
}

fn wait_backend_error(message: &str) -> RuntimeError {
    if message.to_lowercase().contains("timed out") {
        timeout_error(message)
    } else {
        capability_error(format!("frame wait is unavailable: {message}"))
    }
}

struct WaitEvidence<'a> {
    frame: Option<&'a crate::capture::FrameMetadata>,
    observation_id: Option<&'a str>,
    dimensions: Option<(u32, u32)>,
    changed: Option<bool>,
    stable_for_ms: Option<u64>,
}

fn annotate_action_output_with_progress(
    mut output: ToolOutput,
    target: &TargetRef,
    source: &ObservationRef,
    operation: &ActOperation,
    source_snapshot: &Snapshot,
    replacement_snapshot: Option<&Snapshot>,
    progress: Option<&ActionProgress>,
) -> ToolOutput {
    let post_action = output.structured_content.take();
    let replacement = if replacement_snapshot.is_some() {
        post_action.clone().unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let (effect_status, effect_evidence, observed_change) = match (operation, replacement_snapshot) {
        (
            ActOperation::Semantic {
                action: ElementAction::Focus,
                element_id,
            },
            Some(current),
        ) => {
            let replacement = replacement_element_for_source(source_snapshot, current, element_id);
            let focused = replacement
                .as_ref()
                .map(|(element, _)| element.node.states.contains("focused"));
            let active = Some(current.window.states.contains("active"));
            let status = if focused == Some(true) && active == Some(true) {
                "observed"
            } else {
                "not_observed"
            };
            (
                status,
                format!(
                    "replacement observation focus state: element_focused={focused:?}, replacement_element_id={:?}, window_active={active:?}",
                    replacement.as_ref().map(|(_, id)| id)
                ),
                Some(!same_snapshot_content(source_snapshot, current)),
            )
        }
        (ActOperation::Semantic { .. }, Some(current)) => (
            "not_checked",
            "replacement observation captured; any observed accessibility change is not attributed to this generic semantic action".into(),
            Some(!same_snapshot_content(source_snapshot, current)),
        ),
        (ActOperation::Semantic { .. }, None) => (
            "unknown",
            "semantic replacement observation was not retained".into(),
            None,
        ),
        _ => (
            "not_checked",
            "input dispatch completed; any observed replacement change is not a target-specific effect predicate".into(),
            replacement_snapshot.map(|current| !same_snapshot_content(source_snapshot, current)),
        ),
    };
    let focus = match operation {
        ActOperation::Semantic {
            action: ElementAction::Focus,
            element_id,
        } => {
            let replacement = replacement_snapshot.and_then(|snapshot| {
                replacement_element_for_source(source_snapshot, snapshot, element_id)
            });
            let element_focused = replacement
                .as_ref()
                .map(|(element, _)| element.node.states.contains("focused"));
            let window_active =
                replacement_snapshot.map(|snapshot| snapshot.window.states.contains("active"));
            json!({
                "requested": true,
                "element_focused": element_focused,
                "window_active": window_active,
                "replacement_element_id": replacement.map(|(_, id)| id),
                "seat_focus": "not_observable",
                "application_delivery": "not_observable",
                "text_delivery": "not_observable",
                "point": null,
                "click": null,
                "barriers": null
            })
        }
        ActOperation::Keyboard { focus, events } => json!({
            "requested": true,
            "point": {"x": focus.x, "y": focus.y},
            "element_focused": null,
            "window_active": null,
            "replacement_element_id": null,
            "seat_focus": "not_observable",
            "application_delivery": "not_observable",
            "text_delivery": "not_observable",
            "click": {
                "requested": true,
                "sent": true,
                "flushed": true
            },
            "barriers": {
                "focus_click_completed": true,
                "between_events_completed": events.len().saturating_sub(1),
                "cleanup_barrier_completed": true,
                "meaning": "protocol_synchronization_only"
            }
        }),
        _ => json!({
            "requested": false,
            "element_focused": null,
            "window_active": null,
            "replacement_element_id": null,
            "seat_focus": "not_observable",
            "application_delivery": "not_observable",
            "text_delivery": "not_observable",
            "point": null,
            "click": null,
            "barriers": null
        }),
    };
    let replacement_observation_id = replacement_snapshot.map(observation_id_for_snapshot);
    let replacement_element_id = match (operation, replacement_snapshot) {
        (ActOperation::Semantic { element_id, .. }, Some(replacement)) => {
            replacement_element_for_source(source_snapshot, replacement, element_id)
                .map(|(_, id)| id)
        }
        _ => None,
    };
    let keyboard_safety = match operation {
        ActOperation::Keyboard { focus, events } => format!(
            " focus=point({},{}) focus_click=requested,sent,flushed barriers=focus_click,phase_sync={},cleanup=completed",
            focus.x,
            focus.y,
            events.len().saturating_sub(1),
        ),
        _ => String::new(),
    };
    let (protocol_request_sent, request_flushed) = match operation {
        ActOperation::Semantic { .. } => (true, false),
        ActOperation::Pointer { .. } | ActOperation::Keyboard { .. } => (true, true),
    };
    let replacement_frame_id = post_action
        .as_ref()
        .and_then(|value| value["screenshot"]["frame_id"].as_str())
        .map(str::to_owned);
    let action_line = format!(
        "\nAction: request_accepted={protocol_request_sent} protocol_request_sent={protocol_request_sent} request_flushed={request_flushed} synchronized=false effect={effect_status} observed_change={observed_change:?} seat_focus=not_observable application_delivery=not_observable text_delivery=not_observable replacement_observation={replacement_observation_id:?} replacement_frame={replacement_frame_id:?} replacement_element_id={replacement_element_id:?}{keyboard_safety}"
    );
    let mut suffix = action_line;
    if let Some(progress) = progress {
        suffix.push('\n');
        suffix.push_str(&compact_action_progress(progress.snapshot()));
    }
    let text_budget = MAX_MODEL_TEXT_BYTES.saturating_sub(suffix.len());
    let (prefix, text_truncated) = fit_text_prefix(&output.text, text_budget);
    output.text = format!("{prefix}{suffix}");
    let mut action_structured = json!({
        "status": "completed",
        "outcome": "completed",
        "target": target.as_json(),
        "source_observation": {
            "observation_id": source.observation_id,
            "frame_id": source.frame_id
        },
        "dispatch": {
            "request_accepted": protocol_request_sent,
            "protocol_request_sent": protocol_request_sent,
            "request_flushed": request_flushed,
            "synchronized": false,
            "client_delivery": "not_observable"
        },
        "effect": {
            "status": effect_status,
            "evidence": effect_evidence,
            "observed_change": observed_change
        },
        "focus": focus,
        "delivery": {
            "application_delivery": "not_observable",
            "text_delivery": "not_observable"
        },
        "replacement_observation_id": replacement_observation_id,
        "replacement_frame_id": replacement_frame_id,
        "replacement_element_id": replacement_element_id,
        "replacement_observation": replacement
    });
    if replacement_snapshot.is_none() {
        action_structured["post_action"] = json!({
            "replacement_available": false,
            "reason": "post_action_refresh_failed",
            "observation_id": null,
            "frame_id": null,
            "element_ids": []
        });
    }
    if text_truncated {
        action_structured["text_truncation"] = json!({
            "truncated": true,
            "reason": "response_byte_budget",
            "preserved": "complete_text_lines"
        });
    }
    if let Some(progress) = progress {
        action_structured["action_progress"] = action_progress_json(progress.snapshot());
    }
    output.structured_content = Some(bound_action_structured(action_structured));
    output
}

fn fit_text_prefix(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let marker = "Truncated: reason=response_byte_budget text_projection=complete_lines\n";
    let body_budget = max_bytes.saturating_sub(marker.len());
    let mut prefix = String::new();
    for line in text.split_inclusive('\n') {
        if prefix.len().saturating_add(line.len()) > body_budget {
            break;
        }
        prefix.push_str(line);
    }
    if prefix.len().saturating_add(marker.len()) <= max_bytes {
        prefix.push_str(marker);
    } else {
        eprintln!("computer-use-mcp: action text budget is smaller than its truncation marker");
    }
    (prefix, true)
}

fn bound_action_structured(mut action: Value) -> Value {
    if json_size(&action) <= MAX_MODEL_STRUCTURED_BYTES {
        return action;
    }
    let Some(replacement) = action.get("replacement_observation").cloned() else {
        eprintln!(
            "computer-use-mcp: oversized action structured content has no replacement observation"
        );
        return action;
    };
    let Some(replacement_object) = replacement.as_object() else {
        eprintln!("computer-use-mcp: oversized action replacement observation is not an object");
        return action;
    };
    let Some(elements) = replacement_object.get("elements").and_then(Value::as_array) else {
        eprintln!(
            "computer-use-mcp: oversized action replacement observation has no element array"
        );
        return action;
    };
    let original_action = action.clone();
    action["replacement_observation"] = Value::Null;
    let focused_index = elements.iter().position(|element| {
        element["states"]
            .as_array()
            .is_some_and(|states| states.iter().any(|state| state == "focused"))
    });
    let mut element_order = Vec::with_capacity(elements.len());
    if !elements.is_empty() {
        element_order.push(0);
    }
    if let Some(focused_index) = focused_index.filter(|index| !element_order.contains(index)) {
        element_order.push(focused_index);
    }
    let rest = (0..elements.len())
        .filter(|index| !element_order.contains(index))
        .collect::<Vec<_>>();
    element_order.extend(rest);
    let mut included = Vec::new();
    let mut fields_truncated = false;
    for element_index in element_order {
        let element = &elements[element_index];
        let mut low = 0;
        let mut high = MAX_MODEL_FIELD_CHARS;
        let mut best = None;
        while low <= high {
            let field_limit = low + (high - low) / 2;
            let (candidate_element, element_truncated) =
                bound_structured_element(element, field_limit);
            let mut candidate_elements = included.clone();
            candidate_elements.push(candidate_element.clone());
            let candidate_replacement = replacement_with_elements(
                &replacement,
                candidate_elements,
                ResponseTruncation {
                    truncated: true,
                    fields_truncated: fields_truncated || element_truncated,
                    elements_included: included.len() + 1,
                    elements_omitted: elements.len().saturating_sub(included.len() + 1),
                },
            );
            let mut candidate_action = original_action.clone();
            candidate_action["replacement_observation"] = candidate_replacement;
            if json_size(&candidate_action) <= MAX_MODEL_STRUCTURED_BYTES {
                best = Some((
                    candidate_element,
                    element_truncated || field_limit < MAX_MODEL_FIELD_CHARS,
                ));
                low = field_limit.saturating_add(1);
            } else if field_limit == 0 {
                break;
            } else {
                high = field_limit - 1;
            }
        }
        let Some((element, element_truncated)) = best else {
            break;
        };
        fields_truncated |= element_truncated;
        included.push(element);
    }
    let truncation = ResponseTruncation {
        truncated: included.len() < elements.len() || fields_truncated,
        fields_truncated,
        elements_included: included.len(),
        elements_omitted: elements.len().saturating_sub(included.len()),
    };
    let replacement = replacement_with_elements(&replacement, included, truncation);
    action = original_action;
    action["replacement_observation"] = replacement;
    action["truncation"] = json!({
        "truncated": truncation.truncated,
        "reason": truncation.truncated.then_some("response_byte_budget"),
        "replacement_observation": truncation.json()
    });
    if json_size(&action) > MAX_MODEL_STRUCTURED_BYTES {
        eprintln!(
            "computer-use-mcp: action structured content budget invariant failed after element trimming"
        );
    }
    action
}

fn replacement_with_elements(
    replacement: &Value,
    elements: Vec<Value>,
    truncation: ResponseTruncation,
) -> Value {
    let Some(mut object) = replacement.as_object().cloned() else {
        return replacement.clone();
    };
    object.insert("elements".into(), Value::Array(elements));
    object.insert("truncated".into(), Value::Bool(truncation.truncated));
    let truncation_reason = if truncation.truncated {
        Value::String("response_byte_budget".into())
    } else {
        Value::Null
    };
    object.insert("truncation_reason".into(), truncation_reason.clone());
    object.insert("truncation".into(), truncation.json());
    if let Some(accessibility) = object
        .get_mut("accessibility")
        .and_then(Value::as_object_mut)
    {
        accessibility.insert("truncated".into(), Value::Bool(truncation.truncated));
        accessibility.insert("truncation_reason".into(), truncation_reason);
    }
    Value::Object(object)
}

fn bound_structured_element(element: &Value, field_limit: usize) -> (Value, bool) {
    let Some(mut object) = element.as_object().cloned() else {
        eprintln!("computer-use-mcp: structured observation element is not an object");
        return (element.clone(), false);
    };
    let mut truncated = false;
    for key in ["role", "name", "value", "text"] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            let (value, value_truncated) = response_field(value, field_limit);
            truncated |= value_truncated;
            object.insert(key.into(), Value::String(value));
        }
    }
    if let Some(states) = object.get("states").and_then(Value::as_array) {
        let (values, states_truncated) = bounded_response_strings(
            states.iter().filter_map(Value::as_str),
            field_limit,
            MAX_MODEL_STATES,
        );
        truncated |= states_truncated || values.len() < states.len();
        object.insert("states".into(), json!(values));
        object.insert("states_truncated".into(), Value::Bool(states_truncated));
    }
    if let Some(capabilities) = object
        .get_mut("capabilities")
        .and_then(Value::as_object_mut)
        && let Some(actions) = capabilities.get("named_actions").and_then(Value::as_array)
    {
        let mut bounded = Vec::new();
        for action in actions.iter().take(MAX_MODEL_ACTIONS) {
            let Some(mut action) = action.as_object().cloned() else {
                continue;
            };
            for key in ["name", "description"] {
                if let Some(value) = action.get(key).and_then(Value::as_str) {
                    let (value, value_truncated) = response_field(value, field_limit);
                    truncated |= value_truncated;
                    action.insert(key.into(), Value::String(value));
                }
            }
            bounded.push(Value::Object(action));
        }
        if bounded.len() < actions.len() {
            truncated = true;
        }
        capabilities.insert("named_actions".into(), Value::Array(bounded));
        capabilities.insert("named_actions_truncated".into(), Value::Bool(truncated));
    }
    (Value::Object(object), truncated)
}

fn wait_output(
    target: &TargetRef,
    condition: WaitCondition,
    satisfied: bool,
    evidence: &str,
) -> ToolOutput {
    wait_output_with_evidence(
        target,
        condition,
        satisfied,
        evidence,
        WaitEvidence {
            frame: None,
            observation_id: None,
            dimensions: None,
            changed: None,
            stable_for_ms: None,
        },
    )
}

fn wait_output_with_evidence(
    target: &TargetRef,
    condition: WaitCondition,
    satisfied: bool,
    evidence: &str,
    wait_evidence: WaitEvidence<'_>,
) -> ToolOutput {
    let frame_id = wait_evidence.frame.map(crate::capture::frame_id);
    let text = match (frame_id.as_deref(), wait_evidence.dimensions) {
        (Some(frame_id), Some((width, height))) => {
            format!("{evidence}\nFrame ID: {frame_id} bounds=0<=x<{width},0<=y<{height}")
        }
        (Some(frame_id), None) => format!("{evidence}\nFrame ID: {frame_id}"),
        (None, _) => evidence.to_owned(),
    };
    let kind = if satisfied {
        match &condition {
            WaitCondition::FrameAdvanced { .. }
            | WaitCondition::FrameChanged { .. }
            | WaitCondition::FrameStable { .. } => "frame",
            WaitCondition::AccessibilityAdvanced { .. } => "accessibility",
            WaitCondition::ElementState { .. } | WaitCondition::ElementValue { .. } => "element",
        }
    } else {
        "no_change"
    };
    let scope = if wait_evidence.frame.is_some()
        || matches!(
            condition,
            WaitCondition::FrameAdvanced { .. }
                | WaitCondition::FrameChanged { .. }
                | WaitCondition::FrameStable { .. }
        ) {
        "monitor"
    } else if wait_evidence.observation_id.is_some()
        || matches!(
            condition,
            WaitCondition::AccessibilityAdvanced { .. }
                | WaitCondition::ElementState { .. }
                | WaitCondition::ElementValue { .. }
        )
    {
        "target"
    } else {
        "none"
    };
    ToolOutput::text(text).with_structured_content(json!({
        "target": target.as_json(),
        "condition": wait_condition_json(&condition),
        "satisfied": satisfied,
        "evidence": {
            "kind": kind,
        "scope": scope,
        "frame_id": frame_id,
        "width": wait_evidence.dimensions.map(|(width, _)| width),
        "height": wait_evidence.dimensions.map(|(_, height)| height),
        "dimensions": wait_evidence
                .dimensions
                .map(|(width, height)| json!({"width": width, "height": height})),
            "observation_id": wait_evidence.observation_id,
            "changed": wait_evidence.changed,
            "stable_for_ms": wait_evidence.stable_for_ms
        }
    }))
}

fn wait_condition_json(condition: &WaitCondition) -> serde_json::Value {
    match condition {
        WaitCondition::FrameAdvanced { after_frame_id } => {
            json!({"type": "frame_advanced", "after_frame_id": after_frame_id})
        }
        WaitCondition::FrameChanged { after_frame_id } => {
            json!({"type": "frame_changed", "after_frame_id": after_frame_id})
        }
        WaitCondition::FrameStable { for_ms } => {
            json!({"type": "frame_stable", "for_ms": for_ms})
        }
        WaitCondition::AccessibilityAdvanced {
            after_observation_id,
        } => json!({
            "type": "accessibility_advanced",
            "after_observation_id": after_observation_id
        }),
        WaitCondition::ElementState {
            observation_id,
            element_id,
            state,
        } => json!({
            "type": "element_state",
            "observation_id": observation_id,
            "element_id": element_id,
            "state": state
        }),
        WaitCondition::ElementValue {
            observation_id,
            element_id,
            value,
        } => json!({
            "type": "element_value",
            "observation_id": observation_id,
            "element_id": element_id,
            "value": value
        }),
    }
}

fn desktop_session_error(code: &'static str, message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        code,
        message,
        ToolOutcome::NotStarted,
        false,
        "Disable and re-enable the MCP to request KDE approval again.",
    )
}

fn generated_input_error(message: String) -> RuntimeError {
    if message.starts_with(SESSION_UNAVAILABLE) {
        desktop_session_error("backend_failed", message)
    } else {
        operational_error(message)
    }
}

fn map_attempt_error(error: RuntimeError, progress: ActionProgressSnapshot) -> RuntimeError {
    let outcome = progress.outcome();
    let retryable = matches!(outcome, ToolOutcome::NotStarted);
    let recovery = match outcome {
        ToolOutcome::NotStarted => "No input dispatch was confirmed. Call observe before retrying.",
        ToolOutcome::Unknown => {
            "Input dispatch started but its result is unknown. Call observe and do not retry blindly."
        }
        ToolOutcome::Completed => {
            "Input dispatch completed. Call observe before deciding whether another action is needed."
        }
    };
    with_action_progress_snapshot(
        error.with_execution_status(outcome, retryable, recovery),
        progress,
    )
}

fn completed_cleanup_error(error: RuntimeError, progress: ActionProgressSnapshot) -> RuntimeError {
    with_action_progress_snapshot(
        error.with_execution_status(
            ToolOutcome::Completed,
            false,
            "Input dispatch completed, but cleanup failed. Stop and observe the current desktop before any further action.",
        ),
        progress,
    )
}

fn completed_dispatch_error(error: RuntimeError, progress: ActionProgressSnapshot) -> RuntimeError {
    with_action_progress_snapshot(
        error.with_execution_status(
            ToolOutcome::Completed,
            false,
            "Input dispatch completed but the backend reported an error. Observe the current desktop before any further action.",
        ),
        progress,
    )
}

fn map_input_error(error: InputError, progress: ActionProgressSnapshot) -> RuntimeError {
    map_attempt_error(input_runtime_error(&error), progress)
}

fn input_runtime_error(error: &InputError) -> RuntimeError {
    match error {
        InputError::SessionUnavailable(message) => desktop_session_error("backend_failed", message),
        InputError::Dispatch(message) => operational_error(message),
    }
}

fn post_status_for_error(error: &RuntimeError, accessibility: bool) -> PostStatus {
    let message = error.message.to_ascii_lowercase();
    if message.starts_with(SESSION_UNAVAILABLE) || error.code == "backend_failed" {
        PostStatus::SessionUnavailable
    } else if message.contains("timed out") || error.code == "backend_timeout" {
        PostStatus::Timeout
    } else if message.contains("stream") || message.contains("portal") {
        PostStatus::StreamDegraded
    } else if message.contains("catalog") {
        PostStatus::CatalogRefreshFailed
    } else if accessibility || message.contains("at-spi") || message.contains("accessibility") {
        PostStatus::AccessibilityRefreshFailed
    } else {
        PostStatus::Unavailable
    }
}

fn post_visual_error_status(error: &RuntimeError) -> PostStatus {
    match error.code {
        "backend_timeout" => PostStatus::Timeout,
        "backend_failed" if error.message.starts_with(SESSION_UNAVAILABLE) => {
            PostStatus::SessionUnavailable
        }
        _ => PostStatus::CaptureFailed,
    }
}

fn post_accessibility_status(snapshot: &Snapshot) -> PostStatus {
    if snapshot.accessibility_ready {
        PostStatus::Observed
    } else if snapshot.accessibility_reason.as_deref() == Some("accessibility was not requested") {
        PostStatus::NotRun
    } else {
        PostStatus::Unavailable
    }
}

fn state_required_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::not_started("state_required", message)
}

fn stale_catalog_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::stale(StaleAuthority::Catalog, message)
}

fn stale_observation_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::stale(StaleAuthority::Observation, message)
}

fn capability_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::not_started("capability_unavailable", message)
}

fn timeout_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        "backend_timeout",
        message,
        ToolOutcome::NotStarted,
        true,
        "Call observe for current state before retrying.",
    )
}

fn launch_in_progress_error() -> RuntimeError {
    RuntimeError::new(
        "backend_failed",
        "desktop application launch is still in progress",
        ToolOutcome::NotStarted,
        true,
        "Wait for launch completion, then call observe before retrying.",
    )
}

fn uncertain_action(error: RuntimeError) -> RuntimeError {
    error.with_execution_status(
        ToolOutcome::Unknown,
        false,
        "Call observe and inspect the current state before deciding whether to retry.",
    )
}

fn completed_without_observation(error: RuntimeError) -> RuntimeError {
    error.with_execution_status(
        ToolOutcome::Completed,
        false,
        "The action completed, but refresh failed. Call observe and do not repeat the action blindly.",
    )
}

fn completed_post_visual_failure(status: PostStatus, reason: Option<&str>) -> RuntimeError {
    let reason = reason.map_or_else(String::new, |reason| format!("; reason={reason}"));
    RuntimeError::new(
        "post_visual_failed",
        format!(
            "the action dispatch completed, but its requested post-action visual observation failed (visual={}){reason}",
            status.as_str()
        ),
        ToolOutcome::Completed,
        false,
        "The action completed. Observe the exact target again to recover current state, and do not repeat the action based on this failure.",
    )
}

fn cached_element<'a>(
    snapshot: &'a Snapshot,
    index: &str,
) -> Result<&'a ElementSnapshot, RuntimeError> {
    let parsed = snapshot
        .element_ids
        .iter()
        .position(|candidate| candidate == index)
        .ok_or_else(|| {
            operational_error(format!(
                "element_id {index:?} is not an opaque ID from this observation"
            ))
        })?;
    let element = snapshot.elements.get(parsed).ok_or_else(|| {
        operational_error(format!(
            "element_id {parsed} is not in generation {}",
            snapshot.generation
        ))
    })?;
    ensure_element_presented(snapshot, element, index)?;
    Ok(element)
}

fn ensure_element_presented(
    snapshot: &Snapshot,
    element: &ElementSnapshot,
    index: &str,
) -> Result<(), RuntimeError> {
    let query = snapshot.element_query.as_deref().map(str::to_lowercase);
    if element_is_presented_with_query(snapshot.view, query.as_deref(), element) {
        return Ok(());
    }
    Err(operational_error(format!(
        "element_id {index} is no longer included in this observation view"
    )))
}

fn relocated_element<'a>(
    cached: &Snapshot,
    current: &'a Snapshot,
    index: &str,
) -> Result<&'a ElementSnapshot, RuntimeError> {
    let target = relocate(cached_element(cached, index)?, &current.elements)?;
    ensure_element_presented(current, target, index)?;
    Ok(target)
}

fn replacement_element_for_source<'a>(
    source: &Snapshot,
    replacement: &'a Snapshot,
    source_id: &str,
) -> Option<(&'a ElementSnapshot, String)> {
    let source_element = cached_element(source, source_id).ok()?;
    let index = replacement
        .elements
        .iter()
        .position(|element| element.node.object == source_element.node.object)?;
    let replacement_id = replacement
        .element_ids
        .get(index)
        .cloned()
        .unwrap_or_else(|| format!("e-{index:016x}"));
    Some((&replacement.elements[index], replacement_id))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        future,
        sync::{Arc, atomic::AtomicUsize},
    };

    use super::*;
    use crate::{
        runtime::CleanupStatus,
        screenshot::{ScreenshotError, ScreenshotObservation},
        validation::{
            DEFAULT_DESKTOP_PAGE_SIZE, KeyboardEvent, KeyboardPoint, MouseButton, PointerAction,
        },
    };

    #[test]
    fn action_progress_preserves_dispatch_and_cleanup_truth() {
        let progress = ActionProgress::default();
        assert_eq!(
            progress.snapshot().dispatch_stage,
            DispatchStage::NotStarted
        );
        assert_eq!(progress.snapshot().cleanup, CleanupStatus::NotNeeded);

        progress.mark_started();
        progress.mark_cleanup_completed();
        progress.mark_completed();
        progress.mark_post_visual(PostStatus::Observed);
        progress.mark_post_accessibility(PostStatus::Unavailable);
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.dispatch_stage, DispatchStage::Completed);
        assert_eq!(snapshot.cleanup, CleanupStatus::Completed);
        assert_eq!(snapshot.post_visual, PostStatus::Observed);
        assert_eq!(snapshot.post_accessibility, PostStatus::Unavailable);

        progress.mark_cleanup_failed();
        assert_eq!(progress.snapshot().cleanup, CleanupStatus::Failed);
        let error =
            completed_cleanup_error(operational_error("dispatch failed"), progress.snapshot());
        assert_eq!(error.outcome, ToolOutcome::Completed);
        assert_eq!(
            error.action_progress.as_ref().unwrap()["dispatch_stage"],
            "completed"
        );
    }

    #[test]
    fn visual_post_status_preserves_typed_failure_outcomes() {
        let timeout = RuntimeError::new(
            "backend_timeout",
            "capture timed out",
            ToolOutcome::NotStarted,
            true,
            "retry",
        );
        let session = RuntimeError::new(
            "backend_failed",
            SESSION_UNAVAILABLE,
            ToolOutcome::NotStarted,
            false,
            "restart",
        );
        let capture = RuntimeError::new(
            "stale_state",
            "visual mapping changed",
            ToolOutcome::NotStarted,
            true,
            "observe",
        );
        assert_eq!(post_visual_error_status(&timeout), PostStatus::Timeout);
        assert_eq!(
            post_visual_error_status(&session),
            PostStatus::SessionUnavailable
        );
        assert_eq!(
            post_visual_error_status(&capture),
            PostStatus::CaptureFailed
        );
        assert_eq!(
            ScreenshotError(SESSION_UNAVAILABLE.into()).post_status(),
            PostStatus::SessionUnavailable
        );
        assert_eq!(
            ScreenshotError("timed out waiting for a complete frame".into()).post_status(),
            PostStatus::Timeout
        );
        assert_eq!(
            ScreenshotError("PipeWire stream degraded".into()).post_status(),
            PostStatus::StreamDegraded
        );
        assert_eq!(
            ScreenshotError("PNG encoding failed".into()).post_status(),
            PostStatus::CaptureFailed
        );
    }

    #[tokio::test]
    async fn compact_window_listing_preserves_backend_source_and_capabilities() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let output = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();

        assert!(output.text.contains("Backends:"));
        assert!(output.text.contains("source=atspi"));
        assert!(output.text.contains(
            "capabilities=screenshot:supported accessibility:supported activation:supported"
        ));
        let structured = output.structured_content.expect("canonical listing");
        assert!(structured["backends"].is_object());
        assert!(structured["windows"].is_array());
    }

    #[tokio::test]
    async fn atspi_activation_dispatches_even_when_already_active_without_verified_claims() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        let listed = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        let target_value = listed.structured_content.unwrap()["windows"][0]["target"].clone();
        let target = TargetRef {
            app_instance_id: target_value["app_instance_id"]
                .as_str()
                .expect("app target")
                .into(),
            window_instance_id: target_value["window_instance_id"]
                .as_str()
                .expect("window target")
                .into(),
        };

        let output = runtime
            .execute_call(ToolCall::ActivateWindow {
                target: target.clone(),
            })
            .await
            .unwrap();
        let structured = output.structured_content.expect("activation evidence");
        assert_eq!(structured["backend"], "atspi");
        assert_eq!(structured["status"], "atspi_active_observed");
        assert_eq!(structured["before_active"], true);
        assert_eq!(structured["after_active"], true);
        assert_eq!(structured["atspi_active_observed"], true);
        assert_eq!(structured["protocol_state_verified"], false);
        assert_eq!(structured["seat_focus"], "not_observable");
        assert_eq!(structured["dispatch"]["request_accepted"], true);
        assert_eq!(structured["dispatch"]["protocol_request_sent"], true);
        assert_eq!(structured["dispatch"]["request_flushed"], false);
        assert!(structured.get("verified").is_none());
        assert!(output.text.contains("status=atspi_active_observed"));
        assert_eq!(fake.state.lock().unwrap().activation_calls, [id("root")]);
    }

    #[test]
    fn formatting_escapes_truncates_and_reports_focus_selection_and_limits() {
        let mut node = node("root", "button", "line\r\n\"name");
        node.text = Some("é🙂tail".into());
        node.selected_text = Some("a\nb".into());
        node.states.insert("focused".into());
        node.window_frame = Some(Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });
        node.capabilities = inspected(ActionCapabilities::Inspected(vec![
            ActionInfo {
                name: String::new(),
                description: String::new(),
            },
            ActionInfo {
                name: "menu".into(),
                description: "Show\nmenu".into(),
            },
        ]));
        let snapshot = Snapshot {
            view: AccessibilityScope::Full,
            element_query: None,
            app: app("Editor", 1, "Main"),
            window: window("Main", &["active"]),
            generation: 2,
            elements: vec![ElementSnapshot { depth: 0, node }],
            element_ids: Vec::new(),
            node_limit_reached: true,
            depth_limit_reached: true,
            limits: SnapshotLimits {
                text: 2,
                nodes: 10,
                depth: 10,
            },
            target_ref: None,
            accessibility_ready: true,
            accessibility_reason: None,
            requires_atspi_revalidation: true,
            screenshot_requested: true,
        };
        let text = format_snapshot(&snapshot);
        assert!(text.contains("line\\r\\n\\\"name"));
        assert!(text.contains("value=\"é🙂…\""));
        assert!(text.contains("named_actions=[{name=\"menu\", description=\"Show\\nmenu\"}]"));
        assert!(text.contains("Focused element: e-0000000000000000"));
        assert!(text.contains("Selected text: \"a\\n…\""));
        assert!(text.contains("node limit"));
        assert!(text.contains("depth limit"));
        assert!(!text.contains("PNG unavailable"));
    }

    #[test]
    fn observation_puts_png_bounds_before_accessibility_frames() {
        let snapshot = snapshot_for_target(1, "content");
        let mapping = test_screenshot_mapping(&snapshot, "/session/bounds", None);
        let output = observation_output(
            &snapshot,
            true,
            None,
            Some((1280, 853)),
            None,
            Some(&mapping.source),
        );
        assert!(output.text.starts_with("PNG frame_id=frame-"));
        assert!(output.text.contains("bounds=0<=x<1280,0<=y<853\n"));
    }

    #[test]
    fn ready_screenshot_text_exposes_frame_id_without_source_headers() {
        let snapshot = snapshot_for_target(1, "content");
        let mut mapping = test_screenshot_mapping(&snapshot, "/session/frame-text", None);
        mapping.source.source_sequence = None;
        mapping.source.pts_ns = None;
        mapping.source.timestamp_authority = crate::capture::TimestampAuthority::Unavailable;
        let frame_id = crate::capture::frame_id(&mapping.source);

        let output = observation_output(
            &snapshot,
            true,
            None,
            Some(mapping.output_size),
            None,
            Some(&mapping.source),
        );

        assert!(
            output
                .text
                .contains(&format!("PNG frame_id={frame_id} bounds="))
        );
        let structured = output.structured_content.expect("structured observation");
        assert_eq!(structured["screenshot"]["frame_id"], frame_id);
        assert_eq!(
            structured["screenshot"]["metadata"]["source_sequence"],
            Value::Null
        );
        assert_eq!(structured["screenshot"]["metadata"]["pts_ns"], Value::Null);
    }

    #[test]
    fn frame_wait_text_exposes_retained_frame_id() {
        let snapshot = snapshot_for_target(1, "content");
        let mapping = test_screenshot_mapping(&snapshot, "/session/wait-text", None);
        let frame_id = crate::capture::frame_id(&mapping.source);
        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };

        let output = wait_output_with_evidence(
            &target,
            WaitCondition::FrameAdvanced {
                after_frame_id: frame_id.clone(),
            },
            true,
            "a later capture frame was acquired",
            WaitEvidence {
                frame: Some(&mapping.source),
                observation_id: None,
                dimensions: Some(mapping.output_size),
                changed: Some(true),
                stable_for_ms: None,
            },
        );

        assert!(output.text.contains(&format!("Frame ID: {frame_id}")));
        assert_eq!(
            output.structured_content.expect("structured wait evidence")["evidence"]["frame_id"],
            frame_id
        );
    }

    #[test]
    fn click_uses_a_recognized_primary_or_single_unnamed_fallback() {
        let mut target = element(node("button", "button", "Menu"), 1, None);
        target.node.capabilities = inspected(ActionCapabilities::Inspected(vec![
            ActionInfo {
                name: "show menu".into(),
                description: String::new(),
            },
            ActionInfo {
                name: "activate".into(),
                description: String::new(),
            },
        ]));
        assert_eq!(
            semantic_action(ElementAction::Invoke, &target).unwrap(),
            SemanticAction::InvokeAction(1)
        );
        target.node.capabilities = inspected(ActionCapabilities::Inspected(vec![ActionInfo {
            name: String::new(),
            description: String::new(),
        }]));
        assert_eq!(
            semantic_action(ElementAction::Invoke, &target).unwrap(),
            SemanticAction::InvokeAction(0)
        );
        target.node.capabilities = inspected(ActionCapabilities::Inspected(vec![
            ActionInfo {
                name: String::new(),
                description: String::new(),
            },
            ActionInfo {
                name: String::new(),
                description: String::new(),
            },
        ]));
        assert!(semantic_action(ElementAction::Invoke, &target).is_err());
        target.node.capabilities = inspected(ActionCapabilities::Inspected(vec![ActionInfo {
            name: "custom".into(),
            description: String::new(),
        }]));
        assert!(semantic_action(ElementAction::Invoke, &target).is_err());
        target.node.capabilities = inspected(ActionCapabilities::Inspected(vec![]));
        assert!(semantic_action(ElementAction::Invoke, &target).is_err());
        target.node.capabilities = NodeCapabilities::Inspected(InspectedCapabilities {
            actions: ActionCapabilities::InspectionFailed,
            component: true,
            editable_text: false,
            value: false,
        });
        assert!(semantic_action(ElementAction::Focus, &target).is_err());
        assert!(!text_capabilities(&target).contains(&"focus".into()));
        assert_eq!(element_capabilities(0, &target)["focus"], false);
        assert_eq!(
            element_capabilities(0, &target)["element_id"],
            "e-0000000000000000"
        );
        target.node.states.insert("focusable".into());
        assert_eq!(
            semantic_action(ElementAction::Focus, &target).unwrap(),
            SemanticAction::GrabFocus
        );
        assert!(semantic_action(ElementAction::Invoke, &target).is_err());
        target.node.capabilities = NodeCapabilities::InspectionFailed;
        assert!(semantic_action(ElementAction::Focus, &target).is_err());
    }

    #[test]
    fn relocation_requires_exact_object_identity() {
        let old = element(
            node("old", "button", "Save"),
            1,
            Some(Rect {
                x: 1,
                y: 1,
                width: 10,
                height: 10,
            }),
        );
        let replaced = element(node("new", "button", "Save"), 1, old.node.window_frame);
        assert!(
            relocate(&old, &[replaced])
                .unwrap_err()
                .message
                .contains("identity changed")
        );
        let mut defunct = old.clone();
        defunct.node.states.insert("defunct".into());
        assert!(
            relocate(&old, &[defunct])
                .unwrap_err()
                .message
                .contains("defunct")
        );
    }

    #[tokio::test]
    async fn traversal_is_depth_first_deterministic_and_bounded() {
        let fake = FakeAdapter::tree();
        {
            let mut state = fake.state.lock().unwrap();
            let label = node("button-label", "label", "Button label");
            state.nodes.insert(label.object.clone(), label);
            state
                .nodes
                .get_mut(&id("button"))
                .unwrap()
                .children
                .push(id("button-label"));
        }
        let runtime = fake_runtime(fake.clone());
        runtime
            .snapshot_text("Editor".into(), None, Some(3), Some(1))
            .await
            .unwrap();
        let snapshot = current_snapshot(&runtime);
        assert_eq!(
            snapshot
                .elements
                .iter()
                .map(|element| element.node.name.as_str())
                .collect::<Vec<_>>(),
            ["Main", "Button", "Editor"]
        );
        assert_eq!(snapshot.elements[1].depth, 1);
        assert_eq!(snapshot.elements[2].depth, 1);
        assert!(snapshot.node_limit_reached);
        assert!(snapshot.depth_limit_reached);
        assert_eq!(snapshot.elements[1].node.window_frame.unwrap().x, 10);
    }

    #[tokio::test]
    async fn visible_view_prunes_hidden_document_subtrees() {
        let fake = FakeAdapter::tree();
        {
            let mut state = fake.state.lock().unwrap();
            let mut hidden = node("background-document", "document web", "Background tab");
            hidden.states.insert("visible".into());
            hidden.children = vec![id("private-link")];
            let link = node("private-link", "link", "Hidden account");
            state.nodes.insert(hidden.object.clone(), hidden);
            state.nodes.insert(link.object.clone(), link);
            state
                .nodes
                .get_mut(&id("root"))
                .unwrap()
                .children
                .push(id("background-document"));
        }
        let runtime = fake_runtime(fake);

        let full = requested_snapshot(&runtime, ObserveView::Both, None).await;
        assert!(
            full.elements
                .iter()
                .any(|element| element.node.name == "Hidden account")
        );

        let visible = requested_snapshot(&runtime, ObserveView::Accessibility, None).await;
        assert!(
            visible
                .elements
                .iter()
                .all(|element| element.node.name != "Background tab"
                    && element.node.name != "Hidden account")
        );
    }

    #[tokio::test]
    async fn interactive_query_is_compact_and_preserves_element_ids() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let snapshot =
            requested_snapshot(&runtime, ObserveView::Accessibility, Some("button".into())).await;
        let output = observation_output(&snapshot, false, Some("test"), None, None, None);
        let elements = output.structured_content.unwrap()["elements"]
            .as_array()
            .unwrap()
            .clone();
        let button_id = snapshot.element_ids[1].clone();
        let hidden_id = snapshot.element_ids[2].clone();

        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["element_id"], button_id);
        assert_eq!(elements[0]["name"], "Button");
        assert!(
            output
                .text
                .contains(&format!("{button_id}: button name=\"Button\""))
        );
        assert!(!output.text.contains(&format!("{hidden_id}: text")));
        assert_eq!(
            cached_element(&snapshot, &button_id).unwrap().node.name,
            "Button"
        );
        let numeric = cached_element(&snapshot, "1").unwrap_err();
        assert!(numeric.message.contains("not an opaque ID"), "{numeric}");
        let hidden = cached_element(&snapshot, &hidden_id).unwrap_err();
        assert!(hidden.message.contains("included"), "{hidden}");

        let mut changed = (*snapshot).clone();
        changed.elements[1].node.role = "text".into();
        changed.elements[1].node.name = "Renamed".into();
        changed.elements[1].node.capabilities = inspected(ActionCapabilities::Unsupported);
        let changed_error =
            ensure_element_presented(&changed, &changed.elements[1], &button_id).unwrap_err();
        assert!(changed_error.message.contains("no longer included"));
    }

    #[tokio::test]
    async fn semantic_action_rejects_element_that_left_the_filtered_view() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        let snapshot = requested_snapshot(
            &runtime,
            ObserveView::Accessibility,
            Some("activate".into()),
        )
        .await;
        let observation_id = observation_id_for_snapshot(&snapshot);
        let element_id = snapshot.element_ids[1].clone();
        {
            let mut state = fake.state.lock().unwrap();
            let NodeCapabilities::Inspected(capabilities) =
                &mut state.nodes.get_mut(&id("button")).unwrap().capabilities
            else {
                panic!("button capabilities should be inspected");
            };
            capabilities.actions = ActionCapabilities::Inspected(vec![ActionInfo {
                name: "default".into(),
                description: "Default".into(),
            }]);
        }

        let error = runtime
            .element_action_for_test(&observation_id, &element_id, ElementAction::Invoke)
            .await
            .unwrap_err();

        assert!(error.message.contains("no longer included"), "{error}");
        assert!(fake.state.lock().unwrap().actions.is_empty());
        assert!(runtime.required_cached(&observation_id).is_ok());
    }

    #[tokio::test]
    async fn observing_another_window_keeps_the_prior_state_cached() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let first = requested_snapshot(&runtime, ObserveView::Both, None).await;
        let first_id = observation_id_for_snapshot(&first);
        let mut reused_objects = (*first).clone();
        reused_objects.app.pid = 11;
        reused_objects.generation = 0;
        let reused_objects = runtime.commit_snapshot(reused_objects).unwrap();
        assert!(runtime.required_cached(&first_id).is_ok());
        assert!(
            runtime
                .required_cached(&observation_id_for_snapshot(&reused_objects))
                .is_ok()
        );

        let mut other = (*first).clone();
        other.app.name = "Browser".into();
        other.app.pid = 20;
        other.app.object = id("browser-app");
        other.window.object = id("browser-window");
        other.generation = 0;

        let second = runtime.commit_snapshot(other).unwrap();

        assert_eq!(
            runtime.required_cached(&first_id).unwrap().app.name,
            "Editor"
        );
        assert_eq!(
            runtime
                .required_cached(&observation_id_for_snapshot(&second))
                .unwrap()
                .app
                .name,
            "Browser"
        );

        {
            let mut cache = runtime.lock_cache().unwrap();
            for cached in &mut cache.observations {
                cached.screenshot_mapping = Some(test_screenshot_mapping(
                    &cached.snapshot,
                    "/session/cache",
                    None,
                ));
            }
        }
        runtime
            .lock_cache()
            .unwrap()
            .invalidate_for_mutation(&first)
            .unwrap();
        let cache = runtime.lock_cache().unwrap();
        assert!(
            cache
                .observations
                .iter()
                .all(|cached| cached.screenshot_mapping.is_none())
        );
    }

    #[tokio::test]
    async fn action_revalidation_keeps_the_observed_background_window() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        let snapshot = requested_snapshot(&runtime, ObserveView::Both, None).await;
        {
            let mut state = fake.state.lock().unwrap();
            state.app.windows[0].states.remove("active");
            let mut other = window("Other", &["active", "showing"]);
            other.object = id("other-root");
            state.app.windows.push(other);
            state
                .nodes
                .insert(id("other-root"), node("other-root", "frame", "Other"));
        }

        let fresh = runtime.fresh_for_action(&snapshot).await.unwrap();
        assert_eq!(fresh.window.object, snapshot.window.object);
        assert_eq!(fresh.window.title, "Main");
    }

    #[test]
    fn cache_byte_budget_evicts_oldest_target() {
        let payload = "x".repeat(MAX_CACHED_SNAPSHOT_STRING_BYTES / 2 + 1);
        let mut cache = Cache::default();
        let first = cache.insert(snapshot_for_target(1, &payload)).unwrap();
        let second = cache.insert(snapshot_for_target(2, &payload)).unwrap();
        assert!(
            cache
                .required(&observation_id_for_snapshot(&first))
                .is_err()
        );
        assert_eq!(
            cache
                .required(&observation_id_for_snapshot(&second))
                .unwrap()
                .snapshot
                .app
                .pid,
            2
        );
    }

    #[test]
    fn cache_rejects_oversized_snapshot_without_replacing_prior_state() {
        let mut cache = Cache::default();
        let prior = cache.insert(snapshot_for_target(1, "small")).unwrap();
        let payload = "x".repeat(MAX_CACHED_SNAPSHOT_STRING_BYTES + 1);
        let error = cache.insert(snapshot_for_target(1, &payload)).unwrap_err();
        assert!(error.message.contains("byte limit"), "{error}");
        assert!(cache.required(&observation_id_for_snapshot(&prior)).is_ok());
    }

    #[test]
    fn replacement_focus_evidence_maps_by_object_identity_to_the_new_element_id() {
        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };
        let mut source = snapshot_for_target(1, "source");
        source.target_ref = Some(target.clone());
        source.element_ids = vec!["e-0000000000000003".into()];
        let mut replacement = source.clone();
        replacement.element_ids = vec!["e-0000000000000004".into()];
        replacement.elements[0].node.states.insert("focused".into());

        let output = annotate_action_output_with_progress(
            ToolOutput::text("done"),
            &target,
            &ObservationRef {
                observation_id: "obs-0000000000000005".into(),
                frame_id: None,
            },
            &ActOperation::Semantic {
                element_id: "e-0000000000000003".into(),
                action: ElementAction::Focus,
            },
            &source,
            Some(&replacement),
            None,
        );
        let structured = output
            .structured_content
            .as_ref()
            .expect("structured output");
        assert_eq!(structured["focus"]["element_focused"], true);
        assert_eq!(
            structured["focus"]["replacement_element_id"],
            "e-0000000000000004"
        );
        assert_eq!(structured["effect"]["observed_change"], true);
        assert_eq!(structured["dispatch"]["request_accepted"], true);
        assert_eq!(structured["dispatch"]["protocol_request_sent"], true);
        assert_eq!(structured["dispatch"]["request_flushed"], false);
        assert_eq!(structured["dispatch"]["synchronized"], false);
        assert!(output.text.contains("Action: request_accepted=true"));
    }

    #[test]
    fn completed_action_without_replacement_does_not_reuse_source_evidence() {
        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };
        let source = snapshot_for_target(1, "source");
        let output = annotate_action_output_with_progress(
            ToolOutput::text("refresh failed").with_structured_content(json!({
                "observation_id": "obs-0000000000000001",
                "frame_id": "frame-0000000000000002",
                "elements": [{"element_id": "e-0000000000000003"}]
            })),
            &target,
            &ObservationRef {
                observation_id: "obs-0000000000000001".into(),
                frame_id: Some("frame-0000000000000002".into()),
            },
            &ActOperation::Pointer {
                action: PointerAction::Move { x: 1.0, y: 2.0 },
            },
            &source,
            None,
            None,
        );
        let structured = output.structured_content.expect("structured action");
        assert_eq!(structured["replacement_observation"], Value::Null);
        assert_eq!(structured["post_action"]["replacement_available"], false);
        assert_eq!(structured["post_action"]["observation_id"], Value::Null);
        assert_eq!(structured["post_action"]["frame_id"], Value::Null);
        assert!(
            structured["post_action"]["element_ids"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn keyboard_action_evidence_reports_point_click_and_unobservable_delivery() {
        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };
        let source = snapshot_for_target(1, "source");
        let output = annotate_action_output_with_progress(
            ToolOutput::text("done"),
            &target,
            &ObservationRef {
                observation_id: "obs-0000000000000005".into(),
                frame_id: Some("frame-0000000000000006".into()),
            },
            &ActOperation::Keyboard {
                focus: KeyboardPoint { x: 12.0, y: 34.0 },
                events: vec![
                    KeyboardEvent::Press("Ctrl+L".into()),
                    KeyboardEvent::Press("Enter".into()),
                ],
            },
            &source,
            Some(&source),
            None,
        );
        let structured = output
            .structured_content
            .as_ref()
            .expect("structured output");
        assert_eq!(structured["focus"]["requested"], true);
        assert_eq!(structured["focus"]["point"]["x"], 12.0);
        assert_eq!(structured["focus"]["point"]["y"], 34.0);
        assert_eq!(structured["focus"]["click"]["requested"], true);
        assert_eq!(structured["focus"]["click"]["sent"], true);
        assert_eq!(structured["focus"]["click"]["flushed"], true);
        assert_eq!(
            structured["focus"]["barriers"]["focus_click_completed"],
            true
        );
        assert_eq!(
            structured["focus"]["barriers"]["between_events_completed"],
            1
        );
        assert_eq!(
            structured["focus"]["barriers"]["cleanup_barrier_completed"],
            true
        );
        assert_eq!(structured["focus"]["seat_focus"], "not_observable");
        assert_eq!(
            structured["delivery"]["application_delivery"],
            "not_observable"
        );
        assert_eq!(structured["delivery"]["text_delivery"], "not_observable");
        assert!(output.text.contains("focus_click=requested,sent,flushed"));
        assert!(output.text.contains("text_delivery=not_observable"));
    }

    #[tokio::test]
    async fn adapter_calls_are_timed_out() {
        let fake = FakeAdapter::tree();
        fake.state.lock().unwrap().block_reads = true;
        let mut config = test_config();
        config.call_timeout = Duration::from_millis(5);
        config.snapshot_timeout = Duration::from_millis(20);
        let runtime = SemanticRuntime::with_config(fake, config);
        let error = runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap_err();
        assert!(error.message.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn public_act_rejects_a_pid_replacement_after_observe() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        let listed = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        let target_value = listed.structured_content.unwrap()["windows"][0]["target"].clone();
        let target = TargetRef {
            app_instance_id: target_value["app_instance_id"].as_str().unwrap().to_owned(),
            window_instance_id: target_value["window_instance_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let observed = runtime
            .execute_call(ToolCall::Observe {
                target: target.clone(),
                view: ObserveView::Accessibility,
                accessibility: None,
            })
            .await
            .unwrap();
        let metadata = observed.structured_content.unwrap();
        let observation_id = metadata["observation_id"].as_str().unwrap().to_owned();
        let element_id = metadata["elements"][0]["element_id"]
            .as_str()
            .unwrap()
            .to_owned();

        fake.state.lock().unwrap().app.pid = 11;
        let error = runtime
            .execute_call(ToolCall::Act {
                target,
                source: ObservationRef {
                    observation_id,
                    frame_id: None,
                },
                operation: ActOperation::Semantic {
                    element_id,
                    action: ElementAction::Invoke,
                },
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "stale_state");
        assert_eq!(error.outcome, ToolOutcome::NotStarted);
        assert!(fake.state.lock().unwrap().actions.is_empty());
    }

    #[tokio::test]
    async fn unchanged_target_observation_survives_unrelated_catalog_membership_change() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        let snapshot = requested_snapshot(&runtime, ObserveView::Accessibility, None).await;
        let observation_id = observation_id_for_snapshot(&snapshot);
        let element_id = snapshot.element_ids[1].clone();
        {
            let mut state = fake.state.lock().unwrap();
            state.app.windows.push(WindowInfo {
                object: id("unrelated-window"),
                title: "Unrelated".into(),
                states: ["showing".into()].into_iter().collect(),
            });
        }

        let output = runtime
            .execute_call(semantic_call(
                observation_id,
                element_id,
                ElementAction::Invoke,
            ))
            .await
            .expect("unrelated membership must not stale the exact target");

        assert_eq!(fake.state.lock().unwrap().actions.len(), 1);
        assert_eq!(
            output.structured_content.unwrap()["action_progress"]["post_visual"],
            "not_run"
        );
    }

    #[tokio::test]
    async fn launch_invalidation_preserves_existing_target_ids_and_clears_observations() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let snapshot = requested_snapshot(&runtime, ObserveView::Accessibility, None).await;
        let target = snapshot.target_ref.clone().expect("exact target");
        let before = runtime.target_entry(&target).unwrap();
        assert!(!runtime.lock_cache().unwrap().observations.is_empty());

        runtime.invalidate_for_launch().unwrap();
        runtime.refresh_window_catalog().await.unwrap();

        let after = runtime.target_entry(&target).unwrap();
        assert!(runtime.lock_cache().unwrap().observations.is_empty());
        assert_eq!(after.target, before.target);
        assert_eq!(after.backend_identity, before.backend_identity);
        assert_eq!(after.source, before.source);
        assert_eq!(after.pid, before.pid);
    }

    #[tokio::test]
    async fn list_desktop_structured_windows_retain_source_and_capabilities() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let output = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        let structured = output.structured_content.unwrap();
        let windows = structured["windows"]
            .as_array()
            .expect("structured windows array");
        let window = windows
            .iter()
            .find(|window| window["source"]["authority"] == "atspi")
            .expect("AT-SPI window entry");

        assert_eq!(window["source"]["authority"], "atspi");
        assert_eq!(window["source"]["kind"], "atspi");
        assert_eq!(
            window["capabilities"]["accessibility"]["status"],
            "supported"
        );
        assert_eq!(window["capabilities"]["activate"]["status"], "supported");
    }

    #[test]
    fn desktop_page_cursors_are_deterministic_and_fail_closed() {
        let generation = 0x1234_u64;
        let cursor = page_cursor(DesktopScope::Windows, generation, 37);
        assert_eq!(cursor, "cur-windows-0000000000001234-0000000000000025");
        assert_eq!(
            page_start(Some(&cursor), DesktopScope::Windows, generation, 100).unwrap(),
            37
        );
        assert_eq!(
            page_start(
                Some(&page_cursor(DesktopScope::Windows, generation, 100)),
                DesktopScope::Windows,
                generation,
                100,
            )
            .unwrap(),
            100
        );

        for malformed in [
            "",
            "cur-windows",
            "cur-applications-1-2",
            "cur-windows-nope-2",
            "cur-windows-1-0000000000000001",
            "cur-windows-0000000000001234-00000000000000AF",
        ] {
            let error = page_start(Some(malformed), DesktopScope::Windows, generation, 100)
                .expect_err("malformed cursor must fail");
            assert_eq!(error.code, "invalid_arguments", "{malformed:?}");
        }

        let stale = page_start(
            Some(&page_cursor(DesktopScope::Windows, generation + 1, 37)),
            DesktopScope::Windows,
            generation,
            100,
        )
        .expect_err("generation changes must invalidate a cursor");
        assert_eq!(stale.code, "stale_state");
        assert!(stale.recovery.contains("list_desktop again"));
        assert!(!stale.recovery.contains("observe again"));

        let past_end = page_start(
            Some(&page_cursor(DesktopScope::Windows, generation, 101)),
            DesktopScope::Windows,
            generation,
            100,
        )
        .expect_err("an offset past the page boundary must fail");
        assert_eq!(past_end.code, "stale_state");
        assert!(past_end.recovery.contains("list_desktop again"));
    }

    #[test]
    fn desktop_list_text_budget_keeps_entries_whole_and_cursor_exact() {
        let compact = bounded_list_text(
            "Backends: supported",
            &["Window".into()],
            Some("cur-windows-0000000000000001-0000000000000001"),
        );
        assert!(compact.contains("Window\nNext cursor:"));

        let entries = (0..200)
            .map(|index| format!("entry-{index}-{}", "x".repeat(200)))
            .collect::<Vec<_>>();
        let text = bounded_list_text(
            "Backends: standard_foreign_toplevel=supported kde_plasma_rich=unsupported",
            &entries,
            Some("cur-windows-0000000000000001-0000000000000002"),
        );
        assert!(text.len() <= MAX_MODEL_TEXT_BYTES);
        assert!(text.contains("List text truncated: reason=response_byte_budget"));
        assert!(text.contains("Next cursor: cur-windows-0000000000000001-0000000000000002"));
        assert!(text.contains("\nNext cursor: cur-windows-0000000000000001-0000000000000002"));
        assert!(!text.contains("entry-199-"));
    }

    #[tokio::test]
    async fn compact_outputs_report_bounded_text_and_structured_sizes() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let list = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        assert_text_budget("list text", &list.text, 12_000, 500);
        assert_json_budget(
            "list structured",
            list.structured_content.as_ref().expect("list structured"),
            32_000,
        );

        let snapshot = snapshot_for_target(1, "budget");
        let mapping = test_screenshot_mapping(&snapshot, "/session/budget", None);
        let observation = observation_output(
            &snapshot,
            true,
            None,
            Some(mapping.output_size),
            None,
            Some(&mapping.source),
        );
        assert_text_budget("observation text", &observation.text, 24_000, 2_000);
        assert_json_budget(
            "observation structured",
            observation
                .structured_content
                .as_ref()
                .expect("observation structured"),
            48_000,
        );

        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };
        let action = annotate_action_output_with_progress(
            ToolOutput::text("done"),
            &target,
            &ObservationRef {
                observation_id: "obs-0000000000000005".into(),
                frame_id: None,
            },
            &ActOperation::Semantic {
                element_id: "e-0000000000000003".into(),
                action: ElementAction::Focus,
            },
            &snapshot,
            Some(&snapshot),
            None,
        );
        assert_text_budget("action text", &action.text, 4_000, 300);
        assert_json_budget(
            "action structured",
            action
                .structured_content
                .as_ref()
                .expect("action structured"),
            32_000,
        );
    }

    #[test]
    fn worst_case_observation_and_replacement_are_explicitly_truncated() {
        let mut elements = Vec::new();
        for index in 0..240 {
            let mut node = node(
                &format!("worst-{index}"),
                "button",
                &format!("名{}", "name".repeat(2_000)),
            );
            node.value = Some("value🙂".repeat(2_000));
            node.text = Some("text\n\r\"".repeat(2_000));
            node.states.insert("focusable".into());
            if index == 239 {
                node.states.insert("focused".into());
            }
            node.capabilities = inspected(ActionCapabilities::Inspected(
                (0..64)
                    .map(|action| ActionInfo {
                        name: format!("action-{action}-{}", "n".repeat(100)),
                        description: format!("description-{}", "d".repeat(400)),
                    })
                    .collect(),
            ));
            elements.push(ElementSnapshot {
                depth: index % 8,
                node,
            });
        }
        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };
        let snapshot = Snapshot {
            view: AccessibilityScope::Full,
            element_query: None,
            app: app("A", 1, "Main"),
            window: window("Main", &["active"]),
            generation: 7,
            elements,
            element_ids: (0..240).map(|index| format!("e-{index:016x}")).collect(),
            node_limit_reached: true,
            depth_limit_reached: true,
            limits: SnapshotLimits {
                text: usize::MAX,
                nodes: 5_000,
                depth: 128,
            },
            target_ref: Some(target.clone()),
            accessibility_ready: true,
            accessibility_reason: None,
            requires_atspi_revalidation: true,
            screenshot_requested: true,
        };
        let mapping = test_screenshot_mapping(&snapshot, "/session/worst", None);
        let observation = observation_output(
            &snapshot,
            true,
            None,
            Some(mapping.output_size),
            None,
            Some(&mapping.source),
        );
        assert!(observation.text.len() <= MAX_MODEL_TEXT_BYTES);
        let structured = observation
            .structured_content
            .as_ref()
            .expect("structured observation");
        assert_json_budget("worst observation", structured, MAX_MODEL_STRUCTURED_BYTES);
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["truncation"]["reason"], "response_byte_budget");
        assert!(
            structured["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|element| { element["element_id"] == "e-0000000000000000" })
        );
        assert!(
            structured["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|element| { element["element_id"] == "e-00000000000000ef" })
        );
        assert!(
            observation
                .text
                .contains("Truncated: reason=response_byte_budget")
        );

        let progress = Arc::new(ActionProgress::default());
        progress.mark_started();
        progress.mark_cleanup_completed();
        progress.mark_completed();
        progress.mark_post_visual(PostStatus::Observed);
        progress.mark_post_accessibility(PostStatus::Observed);
        let action = annotate_action_output_with_progress(
            observation,
            &target,
            &ObservationRef {
                observation_id: "obs-0000000000000007".into(),
                frame_id: Some(crate::capture::frame_id(&mapping.source)),
            },
            &ActOperation::Semantic {
                element_id: "e-00000000000000ef".into(),
                action: ElementAction::Focus,
            },
            &snapshot,
            Some(&snapshot),
            Some(&progress),
        );
        assert!(action.text.len() <= MAX_MODEL_TEXT_BYTES);
        assert!(action.text.contains("Action: request_accepted=true"));
        assert!(action.text.contains(
            "replacement_observation=Some(\"obs-0000000000000007\") replacement_frame=Some(\"frame-0000000000000007\") replacement_element_id=Some(\"e-00000000000000ef\")"
        ));
        assert!(action.text.ends_with(
            "Action progress: dispatch=completed cleanup=completed visual=observed accessibility=observed"
        ));
        assert!(!action.text.contains("rTruncated:"));
        assert!(!action.text.contains("\"dispatch_stage\""));
        assert!(!action.text.contains("source_sequence"));
        assert!(!action.text.contains("pts_ns"));
        assert_json_budget(
            "worst action",
            action
                .structured_content
                .as_ref()
                .expect("structured action"),
            MAX_MODEL_STRUCTURED_BYTES,
        );
        let structured = action.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "completed");
        assert_eq!(structured["outcome"], "completed");
        assert_eq!(
            structured["replacement_observation_id"],
            "obs-0000000000000007"
        );
        assert_eq!(structured["replacement_frame_id"], "frame-0000000000000007");
        assert_eq!(structured["replacement_element_id"], "e-00000000000000ef");
        assert!(structured["action_progress"].is_object());
    }

    #[tokio::test]
    async fn successful_public_act_projects_bounded_replacement_evidence() {
        struct SuccessfulScreenshots;

        impl ScreenshotProvider for SuccessfulScreenshots {
            async fn prepare(&self) -> Result<(), ScreenshotError> {
                Ok(())
            }

            async fn capture<'a>(
                &'a self,
                snapshot: &'a Snapshot,
            ) -> Result<ScreenshotObservation, ScreenshotError> {
                Ok(ScreenshotObservation {
                    png_base64: "cG5n".into(),
                    mapping: test_screenshot_mapping(snapshot, "/session/act", None),
                })
            }

            async fn perform_input<'a>(
                &'a self,
                _snapshot: &'a Snapshot,
                _mapping: &'a ScreenshotMapping,
                _action: GeneratedInputAction,
                progress: Arc<ActionProgress>,
            ) -> Result<(), InputError> {
                progress.mark_started();
                progress.mark_completed();
                Ok(())
            }
        }

        let runtime = SemanticRuntime::with_screenshot_provider(
            FakeAdapter::tree(),
            SuccessfulScreenshots,
            test_config(),
        );
        let listed = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        let target_value = listed.structured_content.unwrap()["windows"][0]["target"].clone();
        let target = TargetRef {
            app_instance_id: target_value["app_instance_id"].as_str().unwrap().into(),
            window_instance_id: target_value["window_instance_id"].as_str().unwrap().into(),
        };
        let observed = runtime
            .execute_call(ToolCall::Observe {
                target: target.clone(),
                view: ObserveView::Both,
                accessibility: Some(AccessibilityRequest {
                    scope: AccessibilityScope::Full,
                    ..AccessibilityRequest::default()
                }),
            })
            .await
            .unwrap();
        let metadata = observed.structured_content.unwrap();
        let element_id = metadata["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["capabilities"]["invoke"] == true)
            .and_then(|element| element["element_id"].as_str())
            .expect("an invokable element");
        let source = ObservationRef {
            observation_id: metadata["observation_id"].as_str().unwrap().into(),
            frame_id: Some(metadata["screenshot"]["frame_id"].as_str().unwrap().into()),
        };
        let output = runtime
            .execute_call(ToolCall::Act {
                target,
                source,
                operation: ActOperation::Semantic {
                    element_id: element_id.into(),
                    action: ElementAction::Invoke,
                },
            })
            .await
            .unwrap();
        assert!(output.text.len() <= MAX_MODEL_TEXT_BYTES);
        assert!(output.text.contains("Action: request_accepted=true"));
        assert!(output.text.contains("Action progress: dispatch=completed"));
        assert!(!output.text.contains("\"dispatch_stage\""));
        assert!(!output.text.contains("source_sequence"));
        assert!(!output.text.contains("pts_ns"));
        assert_eq!(output.png_base64.as_deref(), Some("cG5n"));
        let complete = serde_json::to_value(output.clone().into_mcp_result())
            .expect("serialize complete public act result");
        assert!(complete["content"][0]["text"].as_str().unwrap().len() <= MAX_MODEL_TEXT_BYTES);
        assert!(
            serde_json::to_vec(&complete["structuredContent"])
                .expect("serialize public act structured content")
                .len()
                <= MAX_MODEL_STRUCTURED_BYTES
        );
        assert!(
            complete["content"][1]["data"].as_str().unwrap().len()
                <= crate::encoder::MAX_PNG_BYTES.div_ceil(3) * 4
        );
        assert!(
            serde_json::to_vec(&complete).unwrap().len()
                <= MAX_MODEL_TEXT_BYTES + MAX_MODEL_STRUCTURED_BYTES + 4_096
        );
        let structured = output.structured_content.expect("successful act evidence");
        assert_json_budget(
            "public act structured",
            &structured,
            MAX_MODEL_STRUCTURED_BYTES,
        );
        assert!(structured["action_progress"].is_object());
        assert!(structured["replacement_observation"].is_object());
        assert!(structured["replacement_observation"]["observation_id"].is_string());
    }

    fn assert_text_budget(label: &str, text: &str, max_bytes: usize, max_words: usize) {
        let words = text.split_whitespace().count();
        assert!(
            text.len() <= max_bytes,
            "{label}: {} bytes exceeds {max_bytes}",
            text.len()
        );
        assert!(
            words <= max_words,
            "{label}: {words} words exceeds {max_words}"
        );
    }

    fn assert_json_budget(label: &str, value: &Value, max_bytes: usize) {
        let bytes = serde_json::to_vec(value).expect("structured content is serializable");
        assert!(
            bytes.len() <= max_bytes,
            "{label}: {} bytes exceeds {max_bytes}",
            bytes.len()
        );
    }

    #[tokio::test]
    async fn inconsistent_optional_interface_metadata_does_not_discard_sound_nodes() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake);
        let text = runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        let snapshot = current_snapshot(&runtime);
        let slider = snapshot
            .elements
            .iter()
            .find(|element| element.node.name == "Zoom")
            .unwrap();
        assert_eq!(
            slider.node.capabilities.set_value_kind(),
            Some(SetValueKind::Number)
        );
        assert!(slider.node.value.is_none());
        assert!(text.contains("name=\"Zoom\""));
        assert!(text.contains("PNG unavailable"));
    }

    #[tokio::test]
    async fn stale_and_defunct_children_are_skipped_with_stable_included_indexes() {
        let fake = FakeAdapter::tree();
        {
            let mut state = fake.state.lock().unwrap();
            state
                .nodes
                .get_mut(&id("root"))
                .unwrap()
                .children
                .insert(1, id("vanished"));
            let mut defunct = node("defunct", "label", "Gone");
            defunct.states.insert("defunct".into());
            state.nodes.insert(defunct.object.clone(), defunct);
            state
                .nodes
                .get_mut(&id("root"))
                .unwrap()
                .children
                .insert(2, id("defunct"));
        }
        let runtime = fake_runtime(fake);
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        let snapshot = current_snapshot(&runtime);
        assert_eq!(
            snapshot
                .elements
                .iter()
                .enumerate()
                .map(|(index, element)| (index, element.node.name.as_str()))
                .collect::<Vec<_>>(),
            [(0, "Main"), (1, "Button"), (2, "Editor"), (3, "Zoom")]
        );
        assert_eq!(snapshot.elements[2].depth, 1);
        assert!(!snapshot.node_limit_reached);
        assert!(!snapshot.depth_limit_reached);
    }

    #[tokio::test]
    async fn every_semantic_action_uses_the_adapter_and_returns_fresh_state() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        let initial_generation = current_snapshot(&runtime).generation;
        assert!(
            semantic_action(
                ElementAction::Named("Show Menu".into()),
                &current_snapshot(&runtime).elements[1],
            )
            .is_err()
        );

        let mut outputs = Vec::new();
        let mut previous_observation_id = current_observation_id(&runtime);
        let mut current_element_ids = current_snapshot(&runtime).element_ids.clone();
        for (element_index, action) in [
            (1, ElementAction::Invoke),
            (1, ElementAction::Named("menu".into())),
            (2, ElementAction::SetValue("  λ\n".into())),
            (3, ElementAction::SetValue("42.5".into())),
        ] {
            let element_id = current_element_ids[element_index].clone();
            let output = runtime
                .execute_call(semantic_call(
                    previous_observation_id.clone(),
                    element_id,
                    action,
                ))
                .await
                .unwrap();
            let next_observation_id = output.structured_content.as_ref().unwrap()
                ["replacement_observation"]["observation_id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_ne!(next_observation_id, previous_observation_id);
            previous_observation_id = next_observation_id;
            current_element_ids = current_snapshot(&runtime).element_ids.clone();
            outputs.push(output);
        }
        assert!(
            outputs
                .iter()
                .all(|output| output.text.contains("PNG unavailable"))
        );
        assert_eq!(
            fake.state.lock().unwrap().actions,
            [
                (id("button"), SemanticAction::InvokeAction(1)),
                (id("button"), SemanticAction::InvokeAction(2)),
                (id("edit"), SemanticAction::ReplaceText("  λ\n".into())),
                (id("slider"), SemanticAction::SetNumericValue(42.5)),
            ]
        );
        let final_state = current_snapshot(&runtime);
        assert_eq!(final_state.generation, initial_generation + 4);
        assert!(fake.state.lock().unwrap().discoveries >= 9);
    }

    #[tokio::test]
    async fn semantic_failure_invalidates_state_before_dispatch() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        let observation_id = current_observation_id(&runtime);
        let element_id = current_snapshot(&runtime).element_ids[1].clone();
        fake.state.lock().unwrap().fail_actions = true;

        let error = runtime
            .execute_call(semantic_call(
                observation_id.clone(),
                element_id.clone(),
                ElementAction::Invoke,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.outcome, ToolOutcome::Unknown);
        assert_eq!(fake.state.lock().unwrap().actions.len(), 1);

        let retry = runtime
            .execute_call(semantic_call(
                observation_id,
                element_id,
                ElementAction::Invoke,
            ))
            .await
            .unwrap_err();
        assert_eq!(retry.code, "state_required");
        assert_eq!(fake.state.lock().unwrap().actions.len(), 1);
    }

    #[tokio::test]
    async fn generated_actions_require_prior_state() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        let error = runtime
            .execute_call(keyboard_call(
                "obs-0000000000000001".into(),
                KeyboardPoint { x: 1.0, y: 2.0 },
                vec![KeyboardEvent::Type("x".into())],
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code, "state_required");
        assert!(error.message.contains("no observation"));

        for call in [
            pointer_call(
                "obs-0000000000000001".into(),
                PointerAction::Move { x: 1.0, y: 2.0 },
            ),
            pointer_call(
                "obs-0000000000000001".into(),
                PointerAction::Click {
                    x: 1.0,
                    y: 2.0,
                    button: MouseButton::Left,
                    count: 1,
                },
            ),
            pointer_call(
                "obs-0000000000000001".into(),
                PointerAction::Drag {
                    path: vec![(0.0, 0.0), (1.0, 1.0)],
                },
            ),
            keyboard_call(
                "obs-0000000000000001".into(),
                KeyboardPoint { x: 1.0, y: 2.0 },
                vec![KeyboardEvent::Press("A".into())],
            ),
            pointer_call(
                "obs-0000000000000001".into(),
                PointerAction::Scroll {
                    x: 1.0,
                    y: 2.0,
                    delta_x: 0,
                    delta_y: 120,
                },
            ),
        ] {
            assert_eq!(
                runtime.execute_call(call).await.unwrap_err().code,
                "state_required"
            );
        }
    }

    #[tokio::test]
    async fn launch_in_progress_rejects_calls_without_backend_work() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime.launch_in_progress.store(true, Ordering::Release);

        let error = runtime.execute_call(observe_call()).await.unwrap_err();
        assert_eq!(error.code, "backend_failed");
        assert_eq!(error.outcome, ToolOutcome::NotStarted);
        assert_eq!(fake.state.lock().unwrap().discoveries, 0);
        assert!(fake.state.lock().unwrap().actions.is_empty());
    }

    #[tokio::test]
    async fn coordinate_scroll_requires_a_live_screenshot_after_state() {
        let runtime = fake_runtime(FakeAdapter::tree());
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        let error = runtime
            .execute_call(pointer_call(
                current_observation_id(&runtime),
                PointerAction::Scroll {
                    x: 1.0,
                    y: 2.0,
                    delta_x: 0,
                    delta_y: 120,
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code, "state_required");
        assert!(error.message.contains("no usable screenshot"));
    }

    #[tokio::test]
    async fn actions_reject_a_stale_observation_id() {
        let runtime = fake_runtime(FakeAdapter::tree());
        runtime.execute_call(observe_call()).await.unwrap();
        let stale_observation_id = current_observation_id(&runtime);
        let stale_element_id = current_snapshot(&runtime).element_ids[1].clone();
        runtime.execute_call(observe_call()).await.unwrap();

        let error = runtime
            .execute_call(semantic_call(
                stale_observation_id,
                stale_element_id,
                ElementAction::Invoke,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code, "stale_state");
        assert!(error.message.contains("is stale"));
    }

    #[test]
    fn frame_ids_require_a_retained_mapping_for_the_exact_target() {
        let target = TargetRef {
            app_instance_id: "app-0000000000000001".into(),
            window_instance_id: "win-0000000000000002".into(),
        };
        let mut snapshot = snapshot_for_target(1, "content");
        snapshot.target_ref = Some(target.clone());
        let mut cache = Cache::default();
        let snapshot = cache.insert(snapshot).unwrap();
        let mapping = test_screenshot_mapping(&snapshot, "/session/frame", Some("frame-map"));
        cache.observations[0].screenshot_mapping = Some(mapping.clone());
        let frame_id = crate::capture::frame_id(&mapping.source);
        assert!(cache.frame_for_target(&target, Some(&frame_id)).is_ok());
        assert!(matches!(
            cache.frame_for_target(&target, Some("frame-ffffffffffffffff")),
            Err(error) if error.code == "stale_state"
        ));
    }

    #[tokio::test]
    async fn actions_reject_stale_pid_changed_window_and_defunct_target() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        fake.state.lock().unwrap().app.pid = 999;
        let error = click(&runtime).await.unwrap_err();
        assert_eq!(error.code, "stale_state");
        assert!(error.message.contains("target"));

        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        {
            let mut state = fake.state.lock().unwrap();
            let mut other = state.nodes[&id("root")].clone();
            other.object = id("other-window");
            state.nodes.insert(other.object.clone(), other);
            state.app.windows[0].object = id("other-window");
        }
        let error = click(&runtime).await.unwrap_err();
        assert_eq!(error.code, "stale_state");
        assert!(error.message.contains("target"));

        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime
            .snapshot_text("Editor".into(), None, None, None)
            .await
            .unwrap();
        fake.state
            .lock()
            .unwrap()
            .nodes
            .get_mut(&id("button"))
            .unwrap()
            .states
            .insert("defunct".into());
        let error = click(&runtime).await.unwrap_err();
        assert_eq!(error.code, "target_unavailable");
        assert!(error.message.contains("defunct"));
    }

    #[derive(Clone, Copy)]
    enum Preparation {
        Fail,
        Panic,
        Pending,
    }

    struct LifecycleScreenshots {
        preparation: Preparation,
        prepares: AtomicUsize,
        started: tokio::sync::Notify,
        shutdowns: AtomicUsize,
    }

    impl LifecycleScreenshots {
        fn new(preparation: Preparation) -> Self {
            Self {
                preparation,
                prepares: AtomicUsize::new(0),
                started: tokio::sync::Notify::new(),
                shutdowns: AtomicUsize::new(0),
            }
        }
    }

    impl ScreenshotProvider for LifecycleScreenshots {
        async fn prepare(&self) -> Result<(), ScreenshotError> {
            self.prepares.fetch_add(1, Ordering::AcqRel);
            match self.preparation {
                Preparation::Fail => Err(ScreenshotError("approval denied".into())),
                Preparation::Panic => panic!("broken initializer"),
                Preparation::Pending => {
                    self.started.notify_one();
                    future::pending().await
                }
            }
        }

        async fn capture<'a>(
            &'a self,
            _snapshot: &'a Snapshot,
        ) -> Result<ScreenshotObservation, ScreenshotError> {
            unreachable!()
        }

        async fn perform_input<'a>(
            &'a self,
            _snapshot: &'a Snapshot,
            _mapping: &'a ScreenshotMapping,
            _action: GeneratedInputAction,
            _progress: Arc<ActionProgress>,
        ) -> Result<(), InputError> {
            unreachable!()
        }

        async fn shutdown_input(&self) -> Result<(), String> {
            self.shutdowns.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[tokio::test]
    async fn desktop_session_initialization_failure_is_stable_and_one_shot() {
        for (preparation, message) in [
            (Preparation::Fail, "approval denied"),
            (Preparation::Panic, "initializer panicked"),
        ] {
            let runtime = SemanticRuntime::with_screenshot_provider(
                FakeAdapter::tree(),
                LifecycleScreenshots::new(preparation),
                test_config(),
            );
            runtime.start_desktop_session();
            runtime.start_desktop_session();

            for _ in 0..2 {
                let error = tokio::time::timeout(Duration::from_secs(1), runtime.desktop_session())
                    .await
                    .expect("initializer failure must wake waiters")
                    .unwrap_err();
                assert_eq!(error.code, "backend_failed");
                assert!(!error.retryable);
                assert!(error.message.contains(message));
            }
            assert_eq!(runtime.screenshots.prepares.load(Ordering::Acquire), 1);
        }
    }

    #[tokio::test]
    async fn screenshot_initialization_timeout_is_reported_as_unavailable_observation() {
        let mut config = test_config();
        config.portal_timeout = Duration::from_millis(1);
        let runtime = SemanticRuntime::with_screenshot_provider(
            FakeAdapter::tree(),
            LifecycleScreenshots::new(Preparation::Pending),
            config,
        );

        let output =
            tokio::time::timeout(Duration::from_secs(1), runtime.execute_call(observe_call()))
                .await
                .expect("screenshot initialization timeout should be bounded")
                .unwrap();

        assert!(output.png_base64.is_none());
        assert!(output.text.contains("PNG unavailable"));
        let structured = output.structured_content.expect("structured observation");
        assert_eq!(structured["screenshot"]["ready"], false);
        assert_eq!(
            structured["screenshot"]["reason"],
            "desktop session initialization timed out"
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_desktop_session_initialization() {
        let runtime = SemanticRuntime::with_screenshot_provider(
            FakeAdapter::tree(),
            LifecycleScreenshots::new(Preparation::Pending),
            test_config(),
        );
        let started = runtime.screenshots.started.notified();
        runtime.start_desktop_session();
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .expect("initializer must start");

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown must not wait for portal timeout")
            .unwrap();
        assert_eq!(runtime.screenshots.prepares.load(Ordering::Acquire), 1);
        assert_eq!(runtime.screenshots.shutdowns.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn generated_input_failure_invalidates_state_before_dispatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FakeScreenshots {
            generated: AtomicUsize,
        }

        impl ScreenshotProvider for FakeScreenshots {
            async fn prepare(&self) -> Result<(), ScreenshotError> {
                Ok(())
            }

            async fn capture<'a>(
                &'a self,
                snapshot: &'a Snapshot,
            ) -> Result<ScreenshotObservation, ScreenshotError> {
                Ok(ScreenshotObservation {
                    png_base64: "cG5n".into(),
                    mapping: test_screenshot_mapping(snapshot, "/session/test", Some("mapping")),
                })
            }

            fn perform_input<'a>(
                &'a self,
                _snapshot: &'a Snapshot,
                _mapping: &'a ScreenshotMapping,
                _action: crate::input::GeneratedInputAction,
                _progress: Arc<ActionProgress>,
            ) -> impl Future<Output = Result<(), InputError>> + Send + 'a {
                self.generated.fetch_add(1, Ordering::AcqRel);
                async { Err("fake does not send generated input".into()) }
            }
        }

        let fake = FakeAdapter::tree();
        let runtime = SemanticRuntime::with_screenshot_provider(
            fake.clone(),
            FakeScreenshots {
                generated: AtomicUsize::new(0),
            },
            test_config(),
        );
        let output = runtime.execute_call(observe_call()).await.unwrap();
        assert_eq!(output.png_base64.as_deref(), Some("cG5n"));
        assert!(!output.text.contains("PNG unavailable"));
        let observation_id = current_observation_id(&runtime);
        let metadata = output.structured_content.as_ref().unwrap();
        assert_eq!(metadata["observation_id"], observation_id);
        assert_eq!(metadata["screenshot"]["ready"], true);
        assert_eq!(metadata["elements"][1]["capabilities"]["invoke"], true);
        assert_eq!(metadata["elements"][2]["capabilities"]["set_value"], "text");
        assert_eq!(
            metadata["elements"][3]["capabilities"]["set_value"],
            "number"
        );
        let mapping = runtime
            .screenshot_mapping(&observation_id)
            .unwrap()
            .unwrap();
        assert_eq!(mapping.stream.mapping_id.as_deref(), Some("mapping"));
        assert_eq!(mapping.accessibility_generation, 1);

        let error = runtime
            .execute_call(spatial_call(
                observation_id.clone(),
                crate::capture::frame_id(&mapping.source),
                ActOperation::Pointer {
                    action: PointerAction::Move { x: 10.0, y: 10.0 },
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code, "target_unavailable");
        assert_eq!(error.outcome, ToolOutcome::NotStarted);
        assert!(error.message.contains("fake does not send generated input"));
        assert!(
            runtime
                .screenshot_mapping(&observation_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(runtime.screenshots.generated.load(Ordering::Acquire), 1);

        let retry = runtime
            .execute_call(spatial_call(
                observation_id,
                crate::capture::frame_id(&mapping.source),
                ActOperation::Pointer {
                    action: PointerAction::Move { x: 10.0, y: 10.0 },
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(retry.code, "state_required");
        assert_eq!(runtime.screenshots.generated.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn typed_alt_tab_is_rejected_before_state_consumption() {
        let fake = FakeAdapter::tree();
        let runtime = fake_runtime(fake.clone());
        runtime.execute_call(observe_call()).await.unwrap();
        let observation_id = current_observation_id(&runtime);

        let error = runtime
            .execute_call(spatial_call(
                observation_id.clone(),
                "frame-0000000000000000".into(),
                ActOperation::Keyboard {
                    focus: KeyboardPoint { x: 10.0, y: 10.0 },
                    events: vec![KeyboardEvent::Press("Alt+Tab".into())],
                },
            ))
            .await
            .unwrap_err();

        assert_eq!(error.code, "unsupported_action");
        assert_eq!(error.outcome, ToolOutcome::NotStarted);
        assert!(!error.retryable);
        assert_eq!(current_observation_id(&runtime), observation_id);
        assert!(fake.state.lock().unwrap().actions.is_empty());
    }

    #[tokio::test]
    async fn keyboard_uses_point_focus_without_atspi_focus_authority() {
        use crate::{input::GeneratedInputAction, screenshot::ScreenshotObservation};

        #[derive(Default)]
        struct RecordingScreenshots {
            actions: Mutex<Vec<GeneratedInputAction>>,
        }

        impl ScreenshotProvider for RecordingScreenshots {
            async fn prepare(&self) -> Result<(), ScreenshotError> {
                Ok(())
            }

            async fn capture<'a>(
                &'a self,
                snapshot: &'a Snapshot,
            ) -> Result<ScreenshotObservation, ScreenshotError> {
                Ok(ScreenshotObservation {
                    png_base64: "cG5n".into(),
                    mapping: test_screenshot_mapping(snapshot, "/session/keyboard", None),
                })
            }

            async fn perform_input<'a>(
                &'a self,
                _snapshot: &'a Snapshot,
                _mapping: &'a ScreenshotMapping,
                action: GeneratedInputAction,
                progress: Arc<ActionProgress>,
            ) -> Result<(), InputError> {
                self.actions.lock().unwrap().push(action);
                progress.mark_started();
                progress.mark_completed();
                Ok(())
            }
        }

        let fake = FakeAdapter::tree();
        let runtime = SemanticRuntime::with_screenshot_provider(
            fake.clone(),
            RecordingScreenshots::default(),
            test_config(),
        );
        runtime.execute_call(observe_call()).await.unwrap();

        let output = runtime
            .execute_call(spatial_call(
                current_observation_id(&runtime),
                crate::capture::frame_id(
                    &runtime
                        .screenshot_mapping(&current_observation_id(&runtime))
                        .unwrap()
                        .unwrap()
                        .source,
                ),
                ActOperation::Keyboard {
                    focus: KeyboardPoint { x: 10.0, y: 20.0 },
                    events: vec![KeyboardEvent::Press("Enter".into())],
                },
            ))
            .await
            .unwrap();

        assert!(fake.state.lock().unwrap().actions.is_empty());
        assert_eq!(
            *runtime.screenshots.actions.lock().unwrap(),
            [GeneratedInputAction::KeyboardTransaction {
                focus: KeyboardPoint { x: 10.0, y: 20.0 },
                events: vec![KeyboardEvent::Press("Enter".into())],
            }]
        );
        assert_eq!(
            output.structured_content.unwrap()["replacement_observation"]["screenshot"]["ready"],
            true
        );
    }

    #[tokio::test]
    async fn stale_keyboard_frame_is_rejected_before_any_point_click_or_key_event() {
        use crate::{input::GeneratedInputAction, screenshot::ScreenshotObservation};

        #[derive(Default)]
        struct RecordingScreenshots {
            actions: Mutex<Vec<GeneratedInputAction>>,
        }

        impl ScreenshotProvider for RecordingScreenshots {
            async fn prepare(&self) -> Result<(), ScreenshotError> {
                Ok(())
            }

            async fn capture<'a>(
                &'a self,
                snapshot: &'a Snapshot,
            ) -> Result<ScreenshotObservation, ScreenshotError> {
                Ok(ScreenshotObservation {
                    png_base64: "cG5n".into(),
                    mapping: test_screenshot_mapping(snapshot, "/session/stale-frame", None),
                })
            }

            async fn perform_input<'a>(
                &'a self,
                _snapshot: &'a Snapshot,
                _mapping: &'a ScreenshotMapping,
                action: GeneratedInputAction,
                progress: Arc<ActionProgress>,
            ) -> Result<(), InputError> {
                self.actions.lock().unwrap().push(action);
                progress.mark_started();
                progress.mark_completed();
                Ok(())
            }
        }

        let runtime = SemanticRuntime::with_screenshot_provider(
            FakeAdapter::tree(),
            RecordingScreenshots::default(),
            test_config(),
        );
        let listed = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        let target_value = listed.structured_content.unwrap()["windows"][0]["target"].clone();
        let target = TargetRef {
            app_instance_id: target_value["app_instance_id"]
                .as_str()
                .expect("app target")
                .into(),
            window_instance_id: target_value["window_instance_id"]
                .as_str()
                .expect("window target")
                .into(),
        };
        let observed = runtime
            .execute_call(ToolCall::Observe {
                target: target.clone(),
                view: ObserveView::Screenshot,
                accessibility: None,
            })
            .await
            .unwrap();
        let observation_id = observed.structured_content.unwrap()["observation_id"]
            .as_str()
            .expect("observation ID")
            .to_owned();

        let error = runtime
            .execute_call(ToolCall::Act {
                target,
                source: ObservationRef {
                    observation_id,
                    frame_id: Some("frame-ffffffffffffffff".into()),
                },
                operation: ActOperation::Keyboard {
                    focus: KeyboardPoint { x: 10.0, y: 20.0 },
                    events: vec![KeyboardEvent::Press("Enter".into())],
                },
            })
            .await
            .unwrap_err();

        assert_eq!(error.code, "stale_state");
        assert!(error.message.contains("source frame is stale"));
        assert!(error.recovery.contains("observe again"));
        assert!(!error.recovery.contains("list_desktop again"));
        assert!(runtime.screenshots.actions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn completed_spatial_action_is_a_tool_error_when_replacement_refresh_fails() {
        let fake = FakeAdapter::tree();
        let runtime = SemanticRuntime::with_screenshot_provider(
            fake.clone(),
            RefreshFailingScreenshots {
                state: Arc::clone(&fake.state),
            },
            test_config(),
        );
        runtime.execute_call(observe_call()).await.unwrap();
        let observation_id = current_observation_id(&runtime);
        let frame_id = crate::capture::frame_id(
            &runtime
                .screenshot_mapping(&observation_id)
                .unwrap()
                .unwrap()
                .source,
        );

        let error = runtime
            .execute_call(spatial_call(
                observation_id,
                frame_id,
                ActOperation::Pointer {
                    action: PointerAction::Move { x: 10.0, y: 10.0 },
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(error.outcome, ToolOutcome::Completed);
        assert!(!error.retryable);
        assert_eq!(
            error.action_progress.as_ref().unwrap()["post_accessibility"],
            "accessibility_refresh_failed"
        );
        assert_eq!(
            error.action_progress.as_ref().unwrap()["post_visual"],
            "not_run"
        );
        let wire = serde_json::to_value(crate::runtime::tool_error_result(&error))
            .expect("serialize MCP tool error");
        assert_eq!(wire["isError"], true);
        assert_eq!(wire["structuredContent"]["code"], error.code);
        assert_eq!(wire["structuredContent"]["outcome"], "completed");
        assert_eq!(wire["structuredContent"]["retryable"], false);
        assert!(
            wire["structuredContent"]["recovery"]
                .as_str()
                .unwrap()
                .contains("Call observe")
        );
        assert!(
            wire["structuredContent"]
                .get("replacement_observation")
                .is_none()
        );
        assert!(
            wire["structuredContent"]
                .get("replacement_observation_id")
                .is_none()
        );
        assert!(
            wire["structuredContent"]
                .get("replacement_frame_id")
                .is_none()
        );
        assert!(
            runtime
                .lock_cache()
                .unwrap()
                .observations
                .iter()
                .all(|cached| cached.snapshot.target_ref.is_none())
        );
    }

    #[tokio::test]
    async fn completed_spatial_action_capture_failed_is_runtime_and_wire_error_without_state() {
        struct PostCaptureFailingScreenshots {
            captures: AtomicUsize,
        }

        impl ScreenshotProvider for PostCaptureFailingScreenshots {
            async fn prepare(&self) -> Result<(), ScreenshotError> {
                Ok(())
            }

            async fn capture<'a>(
                &'a self,
                snapshot: &'a Snapshot,
            ) -> Result<ScreenshotObservation, ScreenshotError> {
                if self.captures.fetch_add(1, Ordering::AcqRel) == 0 {
                    return Ok(ScreenshotObservation {
                        png_base64: "cG5n".into(),
                        mapping: test_screenshot_mapping(
                            snapshot,
                            "/session/post-capture-failure",
                            None,
                        ),
                    });
                }
                Err(ScreenshotError("frame encoder failed".into()))
            }

            async fn perform_input<'a>(
                &'a self,
                _snapshot: &'a Snapshot,
                _mapping: &'a ScreenshotMapping,
                _action: GeneratedInputAction,
                progress: Arc<ActionProgress>,
            ) -> Result<(), InputError> {
                progress.mark_started();
                progress.mark_completed();
                Ok(())
            }
        }

        let runtime = SemanticRuntime::with_screenshot_provider(
            FakeAdapter::tree(),
            PostCaptureFailingScreenshots {
                captures: AtomicUsize::new(0),
            },
            test_config(),
        );
        runtime.execute_call(observe_call()).await.unwrap();
        let observation_id = current_observation_id(&runtime);
        let frame_id = crate::capture::frame_id(
            &runtime
                .screenshot_mapping(&observation_id)
                .unwrap()
                .unwrap()
                .source,
        );

        let error = runtime
            .execute_call(spatial_call(
                observation_id,
                frame_id,
                ActOperation::Pointer {
                    action: PointerAction::Move { x: 10.0, y: 10.0 },
                },
            ))
            .await
            .expect_err("missing requested post-action frame must be a tool error");

        assert_eq!(error.code, "post_visual_failed");
        assert_eq!(error.outcome, ToolOutcome::Completed);
        assert!(!error.retryable);
        assert!(error.recovery.contains("do not repeat"));
        assert_eq!(
            error.action_progress.as_ref().unwrap()["dispatch_stage"],
            "completed"
        );
        assert_eq!(
            error.action_progress.as_ref().unwrap()["post_visual"],
            "capture_failed"
        );
        assert_eq!(
            error.action_progress.as_ref().unwrap()["post_accessibility"],
            "observed"
        );
        let wire = serde_json::to_value(crate::runtime::tool_error_result(&error)).unwrap();
        assert_eq!(wire["isError"], true);
        assert_eq!(wire["structuredContent"]["outcome"], "completed");
        assert_eq!(wire["structuredContent"]["retryable"], false);
        assert!(
            wire["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("frame encoder failed")
        );
        assert_eq!(
            wire["structuredContent"]["action_progress"]["post_visual"],
            "capture_failed"
        );
        for forbidden in [
            "replacement_observation",
            "replacement_observation_id",
            "replacement_frame_id",
            "replacement_element_id",
            "observation_id",
            "frame_id",
            "element_id",
        ] {
            assert!(
                wire["structuredContent"].get(forbidden).is_none(),
                "{forbidden}"
            );
        }
        assert!(
            runtime
                .lock_cache()
                .unwrap()
                .observations
                .iter()
                .all(|cached| cached.snapshot.target_ref.is_none())
        );
    }

    #[tokio::test]
    async fn cleanup_clears_all_screenshot_mappings() {
        let runtime = fake_runtime(FakeAdapter::tree());
        let snapshot = requested_snapshot(&runtime, ObserveView::Both, None).await;
        runtime.lock_cache().unwrap().observations[0].screenshot_mapping =
            Some(test_screenshot_mapping(&snapshot, "/session/cleanup", None));

        DesktopRuntime::cleanup(&runtime, None).await.unwrap();

        assert!(
            runtime.lock_cache().unwrap().observations[0]
                .screenshot_mapping
                .is_none()
        );
    }

    #[tokio::test]
    async fn generated_mutation_serializes_state_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{input::GeneratedInputAction, screenshot::ScreenshotObservation};

        struct ConcurrentScreenshots {
            captures: AtomicUsize,
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
            generated: AtomicUsize,
        }

        impl ScreenshotProvider for ConcurrentScreenshots {
            async fn prepare(&self) -> Result<(), ScreenshotError> {
                Ok(())
            }

            async fn capture<'a>(
                &'a self,
                snapshot: &'a Snapshot,
            ) -> Result<ScreenshotObservation, ScreenshotError> {
                if self.captures.fetch_add(1, Ordering::AcqRel) == 1 {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                Ok(ScreenshotObservation {
                    png_base64: "cG5n".into(),
                    mapping: test_screenshot_mapping(snapshot, "/session/concurrent", None),
                })
            }

            fn perform_input<'a>(
                &'a self,
                _snapshot: &'a Snapshot,
                _mapping: &'a ScreenshotMapping,
                _action: GeneratedInputAction,
                progress: Arc<ActionProgress>,
            ) -> impl Future<Output = Result<(), InputError>> + Send + 'a {
                self.generated.fetch_add(1, Ordering::AcqRel);
                async move {
                    progress.mark_started();
                    progress.mark_completed();
                    Ok(())
                }
            }
        }

        let runtime = Arc::new(SemanticRuntime::with_screenshot_provider(
            FakeAdapter::tree(),
            ConcurrentScreenshots {
                captures: AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                generated: AtomicUsize::new(0),
            },
            test_config(),
        ));
        runtime.execute_call(observe_call()).await.unwrap();
        let initial_observation_id = current_observation_id(&runtime);
        let initial_frame_id = crate::capture::frame_id(
            &runtime
                .screenshot_mapping(&initial_observation_id)
                .unwrap()
                .unwrap()
                .source,
        );
        let mutation_runtime = Arc::clone(&runtime);
        let mutation_observation_id = initial_observation_id.clone();
        let mutation = tokio::spawn(async move {
            mutation_runtime
                .execute_call(spatial_call(
                    mutation_observation_id,
                    initial_frame_id,
                    ActOperation::Pointer {
                        action: PointerAction::Click {
                            x: 10.0,
                            y: 10.0,
                            button: MouseButton::Left,
                            count: 1,
                        },
                    },
                ))
                .await
        });
        runtime.screenshots.entered.notified().await;
        let refresh_runtime = Arc::clone(&runtime);
        let refresh = tokio::spawn(async move {
            refresh_runtime
                .snapshot_text("Editor".into(), None, None, None)
                .await
        });
        tokio::task::yield_now().await;
        assert!(!refresh.is_finished());
        runtime.screenshots.release.notify_one();

        let output = mutation.await.unwrap().unwrap();
        refresh.await.unwrap().unwrap();
        assert_eq!(runtime.screenshots.generated.load(Ordering::Acquire), 1);
        let returned_observation_id = output.structured_content.as_ref().unwrap()
            ["replacement_observation"]["observation_id"]
            .as_str()
            .unwrap();
        assert_ne!(returned_observation_id, initial_observation_id);
        assert_ne!(current_observation_id(&runtime), initial_observation_id);
    }

    #[tokio::test]
    async fn screenshot_revalidation_rejects_window_identity_changes() {
        let fake = FakeAdapter::tree();
        let runtime = SemanticRuntime::with_screenshot_provider(
            fake.clone(),
            MutatingScreenshots {
                state: Arc::clone(&fake.state),
            },
            test_config(),
        );
        let error = runtime.execute_call(observe_call()).await.unwrap_err();
        assert_eq!(error.code, "stale_state");
        assert!(
            runtime
                .screenshot_mapping(&current_observation_id(&runtime))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn targeted_visual_observation_rejects_catalog_removal_before_mapping_commit() {
        let fake = FakeAdapter::tree();
        let runtime = SemanticRuntime::with_screenshot_provider(
            fake.clone(),
            MutatingScreenshots {
                state: Arc::clone(&fake.state),
            },
            test_config(),
        );
        let listed = runtime
            .execute_call(ToolCall::ListDesktop {
                scope: DesktopScope::Windows,
                limit: DEFAULT_DESKTOP_PAGE_SIZE,
                cursor: None,
            })
            .await
            .unwrap();
        let target_value = listed.structured_content.unwrap()["windows"][0]["target"].clone();
        let target = TargetRef {
            app_instance_id: target_value["app_instance_id"].as_str().unwrap().to_owned(),
            window_instance_id: target_value["window_instance_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        };

        let error = runtime
            .execute_call(ToolCall::Observe {
                target,
                view: ObserveView::Screenshot,
                accessibility: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "stale_state");
        assert_eq!(error.outcome, ToolOutcome::NotStarted);
        assert!(
            runtime
                .lock_cache()
                .unwrap()
                .observations
                .iter()
                .all(|cached| cached.screenshot_mapping.is_none())
        );
    }

    async fn click(runtime: &SemanticRuntime<FakeAdapter>) -> Result<ToolOutput, RuntimeError> {
        runtime
            .execute_call(semantic_call(
                current_observation_id(runtime),
                current_snapshot(runtime).element_ids[1].clone(),
                ElementAction::Invoke,
            ))
            .await
    }

    fn test_screenshot_mapping(
        snapshot: &Snapshot,
        session: &str,
        mapping_id: Option<&str>,
    ) -> ScreenshotMapping {
        let crop = crate::geometry::PixelRect {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        ScreenshotMapping {
            app_pid: snapshot.app.pid,
            app_identity: snapshot.app.object.clone(),
            window_identity: snapshot.window.object.clone(),
            accessibility_generation: snapshot.generation,
            portal_session_identity: session.into(),
            portal_session_generation: 1,
            stream: crate::portal::PortalStream {
                stream_index: 0,
                node_id: 1,
                pipewire_serial: Some(2),
                id: Some("stream".into()),
                mapping_id: mapping_id.map(str::to_owned),
                position: Some((0, 0)),
                logical_size: Some((800, 600)),
            },
            source: crate::capture::FrameMetadata {
                generation: snapshot.generation,
                format_generation: 1,
                source_sequence: Some(snapshot.generation),
                pts_ns: Some(i64::try_from(snapshot.generation).unwrap_or_default()),
                arrival_monotonic_ns: snapshot.generation,
                size: (800, 600),
                crop,
                transform: crate::geometry::Transform::Normal,
                timestamp_authority: crate::capture::TimestampAuthority::SpaHeader,
                stream_health: crate::capture::StreamHealth::Healthy,
                content_hash: 0,
                change_epoch: 0,
                changed_from_previous: None,
                sequence_gap: None,
            },
            output_size: (800, 600),
        }
    }

    fn current_snapshot<A, S>(runtime: &SemanticRuntime<A, S>) -> Arc<Snapshot>
    where
        A: AccessibilityAdapter,
        S: ScreenshotProvider,
    {
        Arc::clone(
            &runtime
                .lock_cache()
                .unwrap()
                .observations
                .last()
                .unwrap()
                .snapshot,
        )
    }

    fn current_observation_id<A, S>(runtime: &SemanticRuntime<A, S>) -> String
    where
        A: AccessibilityAdapter,
        S: ScreenshotProvider,
    {
        observation_id_for_snapshot(&current_snapshot(runtime))
    }

    #[derive(Clone)]
    struct FakeAdapter {
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeState {
        app: AppInfo,
        nodes: HashMap<ObjectId, NodeInfo>,
        actions: Vec<(ObjectId, SemanticAction)>,
        activation_calls: Vec<ObjectId>,
        discoveries: usize,
        block_reads: bool,
        fail_actions: bool,
        semantic_focus_succeeds: bool,
        fail_discover: bool,
    }

    struct MutatingScreenshots {
        state: Arc<Mutex<FakeState>>,
    }

    struct RefreshFailingScreenshots {
        state: Arc<Mutex<FakeState>>,
    }

    impl MutatingScreenshots {
        fn mutate(&self) {
            let mut state = self.state.lock().unwrap();
            state.app.windows[0].object = id("replacement-window");
        }
    }

    impl ScreenshotProvider for RefreshFailingScreenshots {
        async fn prepare(&self) -> Result<(), ScreenshotError> {
            Ok(())
        }

        async fn capture<'a>(
            &'a self,
            snapshot: &'a Snapshot,
        ) -> Result<crate::screenshot::ScreenshotObservation, ScreenshotError> {
            Ok(crate::screenshot::ScreenshotObservation {
                png_base64: "cG5n".into(),
                mapping: test_screenshot_mapping(snapshot, "/session/refresh-failure", None),
            })
        }

        async fn perform_input<'a>(
            &'a self,
            _snapshot: &'a Snapshot,
            _mapping: &'a ScreenshotMapping,
            _action: crate::input::GeneratedInputAction,
            progress: Arc<ActionProgress>,
        ) -> Result<(), InputError> {
            self.state.lock().unwrap().fail_discover = true;
            progress.mark_started();
            progress.mark_completed();
            Ok(())
        }
    }

    impl ScreenshotProvider for MutatingScreenshots {
        async fn prepare(&self) -> Result<(), ScreenshotError> {
            Ok(())
        }

        async fn capture<'a>(
            &'a self,
            snapshot: &'a Snapshot,
        ) -> Result<crate::screenshot::ScreenshotObservation, ScreenshotError> {
            self.mutate();
            Ok(crate::screenshot::ScreenshotObservation {
                png_base64: "cG5n".into(),
                mapping: test_screenshot_mapping(snapshot, "/session/revalidation", None),
            })
        }

        async fn perform_input<'a>(
            &'a self,
            _snapshot: &'a Snapshot,
            _mapping: &'a ScreenshotMapping,
            _action: crate::input::GeneratedInputAction,
            _progress: Arc<ActionProgress>,
        ) -> Result<(), InputError> {
            Ok(())
        }
    }

    impl FakeAdapter {
        fn tree() -> Self {
            let mut root = node("root", "frame", "Main");
            root.window_frame = Some(Rect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            });
            root.children = vec![id("button"), id("edit"), id("slider")];

            let mut button = node("button", "button", "Button");
            button.capabilities = inspected(ActionCapabilities::Inspected(vec![
                ActionInfo {
                    name: "default".into(),
                    description: "Default".into(),
                },
                ActionInfo {
                    name: "activate".into(),
                    description: "Activate".into(),
                },
                ActionInfo {
                    name: "menu".into(),
                    description: "Show Menu".into(),
                },
            ]));
            button.window_frame = Some(Rect {
                x: 10,
                y: 20,
                width: 40,
                height: 20,
            });

            let mut edit = node("edit", "text", "Editor");
            edit.capabilities = NodeCapabilities::Inspected(InspectedCapabilities {
                actions: ActionCapabilities::Unsupported,
                component: false,
                editable_text: true,
                value: false,
            });
            edit.states.insert("editable".into());
            edit.states.insert("focused".into());

            let mut slider = node("slider", "slider", "Zoom");
            slider.capabilities = NodeCapabilities::Inspected(InspectedCapabilities {
                actions: ActionCapabilities::Unsupported,
                component: false,
                editable_text: false,
                value: true,
            });

            let nodes = [root, button, edit, slider]
                .into_iter()
                .map(|node| (node.object.clone(), node))
                .collect();
            let mut app = app("Editor", 10, "Main");
            app.windows[0].object = id("root");
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    app,
                    nodes,
                    actions: Vec::new(),
                    activation_calls: Vec::new(),
                    discoveries: 0,
                    block_reads: false,
                    fail_actions: false,
                    semantic_focus_succeeds: true,
                    fail_discover: false,
                })),
            }
        }
    }

    impl AccessibilityAdapter for FakeAdapter {
        async fn discover(&self) -> Result<Vec<AppInfo>, RuntimeError> {
            let mut state = self.state.lock().unwrap();
            state.discoveries += 1;
            if state.fail_discover {
                return Err(operational_error("fake discovery refresh failure"));
            }
            Ok(vec![state.app.clone()])
        }

        async fn read_node<'a>(
            &'a self,
            object: &'a ObjectId,
            _text_limit: usize,
        ) -> Result<NodeInfo, RuntimeError> {
            if self.state.lock().unwrap().block_reads {
                future::pending::<()>().await;
            }
            self.state
                .lock()
                .unwrap()
                .nodes
                .get(object)
                .cloned()
                .ok_or_else(|| operational_error("stale fake object path"))
        }

        async fn act<'a>(
            &'a self,
            object: &'a ObjectId,
            action: SemanticAction,
        ) -> Result<(), RuntimeError> {
            let mut state = self.state.lock().unwrap();
            state.actions.push((object.clone(), action));
            if state.fail_actions {
                return Err(operational_error("fake semantic action failure"));
            }
            if state.semantic_focus_succeeds
                && matches!(state.actions.last(), Some((_, SemanticAction::GrabFocus)))
            {
                for node in state.nodes.values_mut() {
                    node.states.remove("focused");
                }
                state
                    .nodes
                    .get_mut(object)
                    .ok_or_else(|| operational_error("stale fake focus object"))?
                    .states
                    .insert("focused".into());
                state.app.windows[0].states.insert("active".into());
            }
            Ok(())
        }

        async fn activate(&self, object: &ObjectId) -> Result<(), RuntimeError> {
            let mut state = self.state.lock().unwrap();
            state.activation_calls.push(object.clone());
            state.app.windows[0].states.insert("active".into());
            Ok(())
        }
    }

    fn fake_runtime(fake: FakeAdapter) -> SemanticRuntime<FakeAdapter> {
        SemanticRuntime::with_config(fake, test_config())
    }

    async fn requested_snapshot(
        runtime: &SemanticRuntime<FakeAdapter>,
        view: ObserveView,
        query: Option<String>,
    ) -> Arc<Snapshot> {
        let scope = match (view, query.is_some()) {
            (ObserveView::Both, _) => AccessibilityScope::Full,
            (ObserveView::Accessibility, true) => AccessibilityScope::Interactive,
            (ObserveView::Accessibility, false) => AccessibilityScope::Visible,
            (ObserveView::Screenshot, _) => AccessibilityScope::Interactive,
        };
        runtime.refresh_window_catalog().await.unwrap();
        let apps = runtime.discover().await.unwrap();
        let [app] = apps.as_slice() else {
            panic!("test fixture must expose exactly one application");
        };
        let [window] = app.windows.as_slice() else {
            panic!("test fixture must expose exactly one window");
        };
        let binding = AtspiBinding {
            app: app.clone(),
            window: window.clone(),
        };
        let snapshot = runtime
            .collect_snapshot(
                &binding,
                scope,
                query,
                SnapshotLimits {
                    text: runtime.config.default_text_limit,
                    nodes: runtime.config.default_max_nodes,
                    depth: runtime.config.default_max_depth,
                },
            )
            .await
            .unwrap();
        let mut snapshot = snapshot;
        snapshot.target_ref = Some(test_target());
        snapshot.screenshot_requested = matches!(view, ObserveView::Screenshot | ObserveView::Both);
        runtime.commit_snapshot(snapshot).unwrap()
    }

    fn observe_call() -> ToolCall {
        ToolCall::Observe {
            target: test_target(),
            view: ObserveView::Both,
            accessibility: Some(AccessibilityRequest {
                scope: AccessibilityScope::Full,
                ..AccessibilityRequest::default()
            }),
        }
    }

    fn semantic_call(
        observation_id: String,
        element_id: String,
        action: ElementAction,
    ) -> ToolCall {
        ToolCall::Act {
            target: test_target(),
            source: ObservationRef {
                observation_id,
                frame_id: None,
            },
            operation: ActOperation::Semantic { element_id, action },
        }
    }

    fn pointer_call(observation_id: String, action: PointerAction) -> ToolCall {
        spatial_call(
            observation_id,
            "frame-0000000000000000".into(),
            ActOperation::Pointer { action },
        )
    }

    fn spatial_call(observation_id: String, frame_id: String, operation: ActOperation) -> ToolCall {
        ToolCall::Act {
            target: test_target(),
            source: ObservationRef {
                observation_id,
                frame_id: Some(frame_id),
            },
            operation,
        }
    }

    fn keyboard_call(
        observation_id: String,
        focus: KeyboardPoint,
        events: Vec<KeyboardEvent>,
    ) -> ToolCall {
        ToolCall::Act {
            target: test_target(),
            source: ObservationRef {
                observation_id,
                frame_id: Some("frame-0000000000000000".into()),
            },
            operation: ActOperation::Keyboard { focus, events },
        }
    }

    fn test_target() -> TargetRef {
        TargetRef {
            app_instance_id: "app-0000000000000000".into(),
            window_instance_id: "win-0000000000000001".into(),
        }
    }

    fn snapshot_for_target(pid: u32, text: &str) -> Snapshot {
        let mut content = node(&format!("content-{pid}"), "text", "Content");
        content.text = Some(text.into());
        Snapshot {
            view: AccessibilityScope::Full,
            element_query: None,
            app: app(&format!("App {pid}"), pid, "Main"),
            window: window("Main", &["active", "showing"]),
            generation: 0,
            elements: vec![ElementSnapshot {
                depth: 0,
                node: content,
            }],
            element_ids: Vec::new(),
            node_limit_reached: false,
            depth_limit_reached: false,
            limits: SnapshotLimits {
                text: text.len(),
                nodes: 1,
                depth: 1,
            },
            target_ref: None,
            accessibility_ready: true,
            accessibility_reason: None,
            requires_atspi_revalidation: true,
            screenshot_requested: true,
        }
    }

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            call_timeout: Duration::from_millis(100),
            snapshot_timeout: Duration::from_millis(500),
            settle_interval: Duration::ZERO,
            ..RuntimeConfig::default()
        }
    }

    fn app(name: &str, pid: u32, title: &str) -> AppInfo {
        AppInfo {
            object: id(&format!("app-{pid}")),
            name: name.into(),
            pid,
            windows: vec![window(title, &["active", "showing"])],
        }
    }

    fn window(title: &str, states: &[&str]) -> WindowInfo {
        WindowInfo {
            object: id(&format!("window-{title}")),
            title: title.into(),
            states: states.iter().map(|state| (*state).into()).collect(),
        }
    }

    fn id(path: &str) -> ObjectId {
        ObjectId {
            bus_name: ":1.2".into(),
            path: format!("/{path}"),
        }
    }

    fn node(path: &str, role: &str, name: &str) -> NodeInfo {
        NodeInfo {
            object: id(path),
            role: role.into(),
            name: name.into(),
            value: None,
            text: None,
            text_truncated: None,
            selected_text: None,
            states: BTreeSet::new(),
            capabilities: inspected(ActionCapabilities::Unsupported),
            window_frame: None,
            children: vec![],
        }
    }

    fn inspected(actions: ActionCapabilities) -> NodeCapabilities {
        NodeCapabilities::Inspected(InspectedCapabilities {
            actions,
            component: false,
            editable_text: false,
            value: false,
        })
    }

    fn element(mut node: NodeInfo, depth: usize, frame: Option<Rect>) -> ElementSnapshot {
        node.window_frame = frame;
        ElementSnapshot { depth, node }
    }
}
