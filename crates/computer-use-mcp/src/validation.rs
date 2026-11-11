use serde_json::{Map as JsonObject, Value};

use crate::{
    contract::{TOOL_NAMES, tool_definition},
    errors::RuntimeError,
};

pub const MAX_CLICK_COUNT: usize = 3;
pub const MAX_SCROLL_STEPS: u32 = 100;
pub const MAX_TEXT_LIMIT: usize = 100_000;
pub const MAX_TREE_NODES: usize = 5_000;
pub const MAX_TREE_DEPTH: usize = 128;
pub const MAX_QUERY_LENGTH: usize = 1_000;
pub const MAX_DRAG_POINTS: usize = 32;
pub const MAX_KEYBOARD_EVENTS: usize = 8;
pub const MAX_KEYBOARD_TRANSACTION_TEXT: usize = 4_096;
pub const MAX_KEYBOARD_MODIFIERS: usize = 4;
pub const MAX_KEYBOARD_EXPANDED_ACTIONS: usize =
    MAX_KEYBOARD_TRANSACTION_TEXT + MAX_KEYBOARD_EVENTS - 1;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_HUMAN_IDLE_TIMEOUT_MS: u64 = 120_000;
// The stable interval must fit inside the default wait deadline while still
// leaving room to validate capture health and observe subsequent changes.
pub const MAX_WAIT_STABLE_MS: u64 = 1_500;
pub const DEFAULT_DESKTOP_PAGE_SIZE: usize = 50;
pub const MAX_DESKTOP_PAGE_SIZE: usize = 100;
pub const DEFAULT_ACCESSIBILITY_TEXT_LIMIT: usize = 256;
pub const DEFAULT_ACCESSIBILITY_MAX_NODES: usize = 250;
pub const DEFAULT_ACCESSIBILITY_MAX_DEPTH: usize = 64;

pub(crate) enum McpCall {
    Help(Option<&'static rmcp::model::Tool>),
    Dispatch {
        action: &'static str,
        arguments: JsonObject<String, Value>,
    },
}

pub(crate) fn validate_mcp_call(
    tool: &'static rmcp::model::Tool,
    mut arguments: JsonObject<String, Value>,
) -> Result<McpCall, RuntimeError> {
    let name = tool.name.as_ref();
    if !matches!(name, "help" | "dispatch") {
        return Ok(McpCall::Dispatch {
            action: name,
            arguments,
        });
    }
    let action = match arguments.remove("action") {
        None if name == "help" => None,
        value => Some(
            value.as_ref().and_then(Value::as_str).and_then(tool_definition)
                .ok_or_else(|| RuntimeError::invalid_arguments(format!(
                    "Use an advertised desktop action: {}. Call help without action to list their schemas.",
                    TOOL_NAMES.join(", ")
                )))?,
        ),
    };
    let call = if name == "help" {
        McpCall::Help(action)
    } else {
        McpCall::Dispatch {
            action: action.expect("dispatch requires an action").name.as_ref(),
            arguments: required_object(&mut arguments, "arguments")?,
        }
    };
    reject_unknown(arguments)?;
    Ok(call)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Desktop {
    #[default]
    Foreground,
    Background,
}

impl Desktop {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }
}

pub struct RoutedCall {
    pub desktop: Option<Desktop>,
    pub arguments: JsonObject<String, Value>,
    pub call: ToolCall,
}

/// Routing belongs to the parent. Workers receive only the existing, typed
/// desktop-local contract; they never choose a desktop from ambient state.
pub fn validate_routed_call(
    name: &str,
    mut arguments: JsonObject<String, Value>,
) -> Result<RoutedCall, RuntimeError> {
    let desktop = match arguments.remove("desktop") {
        None => None,
        Some(Value::String(value)) if value == "foreground" => Some(Desktop::Foreground),
        Some(Value::String(value)) if value == "background" => Some(Desktop::Background),
        Some(_) => return invalid("desktop must be foreground or background"),
    };
    let call = validate_call(name, arguments.clone())?;
    if desktop.is_some()
        && !matches!(
            call,
            ToolCall::ListDesktop { .. }
                | ToolCall::LaunchApplication { .. }
                | ToolCall::WaitFor {
                    target: None,
                    condition: WaitCondition::WindowOpened { .. },
                    ..
                }
        )
    {
        return invalid(
            "desktop is allowed only for discovery, launch, and targetless window_opened; other calls route by returned IDs",
        );
    }
    Ok(RoutedCall {
        desktop,
        arguments,
        call,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextLimit {
    Count(usize),
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementAction {
    Invoke,
    Named(String),
    Focus,
    SetValue(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PointerAction {
    Move {
        x: f64,
        y: f64,
    },
    Click {
        x: f64,
        y: f64,
        button: MouseButton,
        count: usize,
    },
    Drag {
        path: Vec<(f64, f64)>,
    },
    Scroll {
        x: f64,
        y: f64,
        delta_x: i32,
        delta_y: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopScope {
    Windows,
    Applications,
}

pub use crate::window_backend::WindowTarget as TargetRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveView {
    Screenshot,
    Accessibility,
    Both,
}

impl ObserveView {
    pub const ALL: [Self; 3] = [Self::Screenshot, Self::Accessibility, Self::Both];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Accessibility => "accessibility",
            Self::Both => "both",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObserveCrop {
    #[default]
    Monitor,
    TargetWindow,
}

impl ObserveCrop {
    pub const ALL: [Self; 2] = [Self::Monitor, Self::TargetWindow];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::TargetWindow => "target_window",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|crop| crop.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityScope {
    Full,
    Visible,
    Interactive,
}

impl AccessibilityScope {
    pub const ALL: [Self; 3] = [Self::Full, Self::Visible, Self::Interactive];

    /// The model-facing default is owned by the accessibility domain.  Schema
    /// generation and parsing both derive their defaults from this value.
    pub const DEFAULT: Self = Self::Interactive;

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Visible => "visible",
            Self::Interactive => "interactive",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == value)
    }
}

impl Default for AccessibilityScope {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityLimits {
    pub text: TextLimit,
    pub nodes: usize,
    pub depth: usize,
}

impl Default for AccessibilityLimits {
    fn default() -> Self {
        Self {
            text: TextLimit::Count(DEFAULT_ACCESSIBILITY_TEXT_LIMIT),
            nodes: DEFAULT_ACCESSIBILITY_MAX_NODES,
            depth: DEFAULT_ACCESSIBILITY_MAX_DEPTH,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessibilityRequest {
    pub scope: AccessibilityScope,
    pub query: Option<String>,
    pub search: Option<AccessibilitySearch>,
    pub limits: AccessibilityLimits,
}

/// A bounded native-name/role search, not a persistent element selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilitySearch {
    pub name: Option<String>,
    pub role: Option<String>,
    pub exact: bool,
    pub limit: usize,
    pub max_nodes: usize,
    pub timeout_ms: u64,
}

impl AccessibilitySearch {
    pub fn as_json(&self) -> Value {
        serde_json::json!({"name": self.name, "role": self.role, "exact": self.exact,
            "limit": self.limit, "max_nodes": self.max_nodes, "timeout_ms": self.timeout_ms})
    }

    pub fn string_bytes(&self) -> usize {
        self.name.as_ref().map_or(0, String::len) + self.role.as_ref().map_or(0, String::len)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRef {
    pub observation_id: String,
    pub frame_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyboardPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    Point(KeyboardPoint),
    Semantic {
        element_id: String,
    },
    /// Whatever currently holds keyboard focus in the target window, which
    /// must be the active window. No focus click and no GrabFocus is sent.
    Window,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardEvent {
    Press(String),
    Type(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowAction {
    #[default]
    Activate,
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl WindowAction {
    pub const ALL: [Self; 5] = [
        Self::Activate,
        Self::Minimize,
        Self::Maximize,
        Self::Restore,
        Self::Close,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
            Self::Close => "close",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActOperation {
    Pointer {
        action: PointerAction,
    },
    Semantic {
        element_id: String,
        action: ElementAction,
    },
    Keyboard {
        focus: KeyboardFocus,
        events: Vec<KeyboardEvent>,
    },
    Paste {
        focus: KeyboardFocus,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    HumanIdle,
    FrameAdvanced {
        after_frame_id: String,
    },
    FrameChanged {
        after_frame_id: String,
    },
    FrameStable {
        for_ms: u64,
    },
    AccessibilityAdvanced {
        after_observation_id: String,
    },
    ElementState {
        observation_id: String,
        element_id: String,
        state: String,
    },
    ElementValue {
        observation_id: String,
        element_id: String,
        value: String,
    },
    WindowOpened {
        desktop_id: String,
    },
    WindowClosed {
        window_instance_id: String,
    },
}

impl ActOperation {
    pub(crate) fn source_element_id(&self) -> Option<&str> {
        match self {
            Self::Semantic { element_id, .. }
            | Self::Keyboard {
                focus: KeyboardFocus::Semantic { element_id },
                ..
            }
            | Self::Paste {
                focus: KeyboardFocus::Semantic { element_id },
                ..
            } => Some(element_id),
            _ => None,
        }
    }

    /// Keyboard or paste aimed at the target window's current focus.
    pub(crate) fn window_focus(&self) -> bool {
        matches!(
            self,
            Self::Keyboard {
                focus: KeyboardFocus::Window,
                ..
            } | Self::Paste {
                focus: KeyboardFocus::Window,
                ..
            }
        )
    }
}

impl ToolCall {
    pub const fn requires_visual_session(&self) -> bool {
        match self {
            Self::ListDesktop { .. }
            | Self::LaunchApplication { .. }
            | Self::ActivateWindow { .. } => false,
            Self::Observe { view, .. } => match view {
                ObserveView::Screenshot | ObserveView::Both => true,
                ObserveView::Accessibility => false,
            },
            Self::Act { operation, .. } => match operation {
                // Focused-element typing still needs the live desktop/EIS
                // session; only the screenshot mapping is skipped.
                ActOperation::Pointer { .. }
                | ActOperation::Keyboard { .. }
                | ActOperation::Paste { .. } => true,
                ActOperation::Semantic { .. } => false,
            },
            Self::WaitFor { condition, .. } => match condition {
                WaitCondition::FrameAdvanced { .. }
                | WaitCondition::FrameChanged { .. }
                | WaitCondition::FrameStable { .. } => true,
                WaitCondition::AccessibilityAdvanced { .. }
                | WaitCondition::ElementState { .. }
                | WaitCondition::ElementValue { .. }
                | WaitCondition::WindowOpened { .. }
                | WaitCondition::WindowClosed { .. }
                | WaitCondition::HumanIdle => false,
            },
        }
    }

    pub const fn tracks_action(&self) -> bool {
        matches!(
            self,
            Self::LaunchApplication { .. } | Self::ActivateWindow { .. } | Self::Act { .. }
        )
    }

    pub(crate) fn validate_policy(&self) -> Result<(), RuntimeError> {
        let events = match self {
            Self::Act {
                operation: ActOperation::Keyboard { events, .. },
                ..
            } => events.as_slice(),
            _ => &[],
        };
        for key in events.iter().filter_map(|event| match event {
            KeyboardEvent::Press(key) => Some(key.as_str()),
            KeyboardEvent::Type(_) => None,
        }) {
            let mut parts = key.split('+').map(str::trim);
            let has_alt = parts.clone().any(|part| part.eq_ignore_ascii_case("alt"));
            let has_tab = parts.any(|part| part.eq_ignore_ascii_case("tab"));
            if has_alt && has_tab {
                return Err(RuntimeError::unsupported_desktop_focus_switch());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    ListDesktop {
        scope: DesktopScope,
        limit: usize,
        cursor: Option<String>,
    },
    LaunchApplication {
        desktop_id: String,
    },
    ActivateWindow {
        target: TargetRef,
        action: WindowAction,
    },
    Observe {
        target: TargetRef,
        view: ObserveView,
        accessibility: Option<AccessibilityRequest>,
        crop: ObserveCrop,
    },
    Act {
        target: Option<TargetRef>,
        source: Option<ObservationRef>,
        operation: ActOperation,
    },
    WaitFor {
        target: Option<TargetRef>,
        condition: WaitCondition,
        timeout_ms: u64,
    },
}

pub fn validate_call(
    name: &str,
    mut arguments: JsonObject<String, Value>,
) -> Result<ToolCall, RuntimeError> {
    if name == "act" && arguments.contains_key("operations") {
        return invalid(
            "act applies one \"operation\" object per call, not an \"operations\" array; send each operation in its own call",
        );
    }
    if name == "act" && arguments.contains_key("element_id") {
        return invalid(
            "element_id belongs inside operation, not beside it: {\"operation\":{\"type\":\"semantic\",\"element_id\":\"EL\",\"action\":{\"type\":\"invoke\"}}}",
        );
    }
    if name == "wait_for" && arguments.contains_key("duration_ms") {
        return invalid(
            "wait_for is condition-driven: supply condition and timeout_ms, not duration_ms; request help for wait_for",
        );
    }
    // Bare window IDs beside the other arguments mean the target; fold them
    // into the documented object. Both IDs are still required and checked.
    if matches!(name, "activate_window" | "observe" | "act" | "wait_for")
        && !arguments.contains_key("target")
        && arguments.contains_key("app_instance_id")
        && arguments.contains_key("window_instance_id")
    {
        let mut target = JsonObject::new();
        for key in ["app_instance_id", "window_instance_id"] {
            if let Some(value) = arguments.remove(key) {
                target.insert(key.into(), value);
            }
        }
        arguments.insert("target".into(), Value::Object(target));
    }
    let call = match name {
        "list_desktop" => {
            let scope = match arguments.remove("scope") {
                None => DesktopScope::Windows,
                Some(Value::String(value)) if value == "windows" => DesktopScope::Windows,
                Some(Value::String(value)) if value == "applications" => DesktopScope::Applications,
                _ => return invalid("scope must be \"windows\" or \"applications\""),
            };
            let limit = match arguments.remove("limit") {
                None => DEFAULT_DESKTOP_PAGE_SIZE,
                Some(value) => json_integer(&value)
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| (1..=MAX_DESKTOP_PAGE_SIZE).contains(value))
                    .ok_or_else(|| {
                        RuntimeError::invalid_arguments(format!(
                            "argument \"limit\" must be an integer from 1 through {MAX_DESKTOP_PAGE_SIZE}"
                        ))
                    })?,
            };
            let cursor = match arguments.remove("cursor") {
                None => None,
                Some(Value::String(value)) if !value.is_empty() && value.chars().count() <= 128 => {
                    Some(value)
                }
                Some(Value::String(_)) => {
                    return invalid("argument \"cursor\" must be a non-empty opaque cursor");
                }
                Some(_) => return invalid("argument \"cursor\" must be a string"),
            };
            ToolCall::ListDesktop {
                scope,
                limit,
                cursor,
            }
        }
        "launch_application" => ToolCall::LaunchApplication {
            desktop_id: required_desktop_id(&mut arguments, "desktop_id")?,
        },
        "activate_window" => ToolCall::ActivateWindow {
            target: required_target(&mut arguments, "target")?,
            action: optional_window_action(&mut arguments, "action")?,
        },
        "observe" => ToolCall::Observe {
            target: required_target(&mut arguments, "target")?,
            view: required_observe_view(&mut arguments, "view")?,
            accessibility: optional_accessibility(&mut arguments, "accessibility")?,
            crop: optional_observe_crop(&mut arguments, "crop")?,
        },
        "act" => {
            // A bare observation_id/frame_id beside operation means the source
            // observation; fold it into the documented object shape.
            if !arguments.contains_key("source_observation")
                && let Some(observation_id) = arguments.remove("observation_id")
            {
                let mut source = JsonObject::new();
                source.insert("observation_id".into(), observation_id);
                if let Some(frame_id) = arguments.remove("frame_id") {
                    source.insert("frame_id".into(), frame_id);
                }
                arguments.insert("source_observation".into(), Value::Object(source));
            }
            // The same object nested inside operation is hoisted beside it.
            if !arguments.contains_key("source_observation")
                && let Some(Value::Object(operation)) = arguments.get_mut("operation")
                && let Some(source) = operation.remove("source_observation")
            {
                arguments.insert("source_observation".into(), source);
            }
            ToolCall::Act {
                target: optional_target(&mut arguments, "target")?,
                source: if arguments.contains_key("source_observation") {
                    Some(required_observation_ref(
                        &mut arguments,
                        "source_observation",
                    )?)
                } else {
                    None
                },
                operation: act_operation(required_object(&mut arguments, "operation")?)?,
            }
        }
        "wait_for" => {
            let condition = wait_condition(required_object(&mut arguments, "condition")?)?;
            let target = optional_target(&mut arguments, "target")?;
            validate_wait_target(target.as_ref(), &condition)?;
            let maximum = if condition == WaitCondition::HumanIdle {
                MAX_HUMAN_IDLE_TIMEOUT_MS
            } else {
                MAX_WAIT_TIMEOUT_MS
            };
            ToolCall::WaitFor {
                target,
                condition,
                timeout_ms: required_timeout(&mut arguments, "timeout_ms", maximum)?,
            }
        }
        _ => return invalid(format!("unknown tool {name:?}")),
    };
    reject_unknown(arguments)?;
    call.validate_policy()?;
    validate_action_source(&call)?;
    Ok(call)
}

fn validate_action_source(call: &ToolCall) -> Result<(), RuntimeError> {
    let ToolCall::Act {
        source, operation, ..
    } = call
    else {
        return Ok(());
    };
    // Keyboard/paste without focus needs no source here: the runtime binds it
    // to the named window, or to the window last observed or acted on.
    if source.is_none() && operation.source_element_id().is_none() && !operation.window_focus() {
        return invalid(
            "source_observation is required except for exact retained semantic elements/focus",
        );
    }
    let needs_frame = match operation {
        ActOperation::Pointer { .. } => true,
        ActOperation::Keyboard { focus, .. } | ActOperation::Paste { focus, .. } => {
            matches!(focus, KeyboardFocus::Point(_))
        }
        ActOperation::Semantic { .. } => false,
    };
    if !needs_frame {
        return Ok(());
    }
    if source
        .as_ref()
        .and_then(|source| source.frame_id.as_ref())
        .is_none()
    {
        return invalid(
            "source_observation.frame_id is required for pointer and point-focused keyboard/paste operations",
        );
    }
    Ok(())
}

fn required_target(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<TargetRef, RuntimeError> {
    target_from_object(required_object(arguments, key)?)
}

fn optional_target(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<Option<TargetRef>, RuntimeError> {
    match arguments.remove(key) {
        None => Ok(None),
        Some(Value::Object(object)) => Ok(Some(target_from_object(object)?)),
        Some(_) => Err(RuntimeError::invalid_arguments(format!(
            "argument {key:?} must be an object"
        ))),
    }
}

fn target_from_object(mut object: JsonObject<String, Value>) -> Result<TargetRef, RuntimeError> {
    let target = TargetRef {
        app_instance_id: required_opaque_id(&mut object, "app_instance_id", "app")?,
        window_instance_id: required_opaque_id(&mut object, "window_instance_id", "win")?,
    };
    reject_unknown(object)?;
    Ok(target)
}

fn validate_wait_target(
    target: Option<&TargetRef>,
    condition: &WaitCondition,
) -> Result<(), RuntimeError> {
    match condition {
        WaitCondition::HumanIdle if target.is_some() => {
            invalid("human_idle does not accept a target")
        }
        WaitCondition::HumanIdle => Ok(()),
        WaitCondition::WindowOpened { .. } => Ok(()),
        WaitCondition::WindowClosed { window_instance_id } => {
            if let Some(target) = target
                && target.window_instance_id != *window_instance_id
            {
                return invalid(
                    "target.window_instance_id must match condition.window_instance_id",
                );
            }
            Ok(())
        }
        _ if target.is_none() => {
            invalid("missing required argument \"target\" for this wait condition")
        }
        _ => Ok(()),
    }
}

fn required_opaque_id(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
    prefix: &str,
) -> Result<String, RuntimeError> {
    let value = required_string(arguments, key)?;
    if !is_opaque_id(&value, prefix) {
        return invalid(format!(
            "argument {key:?} must be an opaque {prefix}- followed by 16 lowercase hexadecimal digits"
        ));
    }
    Ok(value)
}

fn required_observe_view(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<ObserveView, RuntimeError> {
    let value = required_string(arguments, key)?;
    ObserveView::parse(&value).ok_or_else(|| {
        RuntimeError::invalid_arguments(format!(
            "argument {key:?} must be screenshot, accessibility, or both"
        ))
    })
}

fn optional_observe_crop(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<ObserveCrop, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(ObserveCrop::default());
    };
    let Value::String(value) = value else {
        return invalid(format!("argument {key:?} must be a string"));
    };
    ObserveCrop::parse(&value).ok_or_else(|| {
        RuntimeError::invalid_arguments(format!(
            "argument {key:?} must be \"monitor\" or \"target_window\""
        ))
    })
}

fn optional_accessibility(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<Option<AccessibilityRequest>, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(Some(AccessibilityRequest::default()));
    };
    let Value::Object(mut object) = value else {
        return invalid(format!("argument {key:?} must be an object"));
    };
    let scope = match object.remove("scope") {
        None => AccessibilityScope::default(),
        Some(Value::String(value)) => AccessibilityScope::parse(&value).ok_or_else(|| {
            RuntimeError::invalid_arguments(
                "accessibility.scope must be full, visible, or interactive",
            )
        })?,
        Some(_) => {
            return invalid("accessibility.scope must be a string");
        }
    };
    let query = match object.remove("query") {
        None => None,
        Some(Value::String(value)) => {
            if value.chars().count() > MAX_QUERY_LENGTH {
                return invalid(format!(
                    "accessibility.query must contain at most {MAX_QUERY_LENGTH} characters"
                ));
            }
            let normalized = value.trim();
            if normalized.is_empty() {
                return invalid("accessibility.query must not be blank");
            }
            Some(normalized.to_owned())
        }
        Some(_) => return invalid("accessibility.query must be a string"),
    };
    let search = match object.remove("search") {
        None => None,
        Some(Value::Object(mut search)) => {
            if query.is_some() {
                return invalid("accessibility.query and search are mutually exclusive");
            }
            let mut field = |key| -> Result<Option<String>, RuntimeError> {
                match search.remove(key) {
                    None => Ok(None),
                    Some(Value::String(value))
                        if !value.trim().is_empty()
                            && value.chars().count() <= MAX_QUERY_LENGTH =>
                    {
                        Ok(Some(value))
                    }
                    _ => invalid(
                        "search name/role must be nonblank strings of at most 1000 characters",
                    ),
                }
            };
            let name = field("name")?;
            let role = field("role")?;
            if name.is_none() && role.is_none() {
                return invalid("search requires name and/or role");
            }
            let exact = match search.remove("exact") {
                None => false,
                Some(Value::Bool(value)) => value,
                _ => return invalid("search.exact must be boolean"),
            };
            let limit = optional_bounded(&mut search, "limit", 100)?.unwrap_or(20);
            let max_nodes =
                optional_bounded(&mut search, "max_nodes", MAX_TREE_NODES)?.unwrap_or(500);
            let timeout_ms =
                optional_bounded(&mut search, "timeout_ms", 2_000_u64)?.unwrap_or(2_000);
            reject_unknown(search)?;
            Some(AccessibilitySearch {
                name,
                role,
                exact,
                limit,
                max_nodes,
                timeout_ms,
            })
        }
        Some(_) => return invalid("accessibility.search must be an object"),
    };
    let limits = match object.remove("limits") {
        None => AccessibilityLimits::default(),
        Some(Value::Object(mut limits)) => {
            let text = match limits.remove("text_limit") {
                None => AccessibilityLimits::default().text,
                Some(Value::String(value)) if value == "max" => TextLimit::Max,
                Some(value) => {
                    let count = json_integer(&value)
                        .and_then(|count| usize::try_from(count).ok())
                        .ok_or_else(|| {
                            RuntimeError::invalid_arguments(
                                "accessibility.limits.text_limit must be an integer or \"max\"",
                            )
                        })?;
                    if count > MAX_TEXT_LIMIT {
                        return invalid(format!(
                            "accessibility.limits.text_limit must not exceed {MAX_TEXT_LIMIT}"
                        ));
                    }
                    TextLimit::Count(count)
                }
            };
            let nodes = optional_bounded(&mut limits, "max_nodes", MAX_TREE_NODES)?
                .unwrap_or(DEFAULT_ACCESSIBILITY_MAX_NODES);
            let depth = optional_bounded(&mut limits, "max_depth", MAX_TREE_DEPTH)?
                .unwrap_or(DEFAULT_ACCESSIBILITY_MAX_DEPTH);
            reject_unknown(limits)?;
            AccessibilityLimits { text, nodes, depth }
        }
        Some(_) => return invalid("accessibility.limits must be an object"),
    };
    reject_unknown(object)?;
    Ok(Some(AccessibilityRequest {
        scope,
        query,
        search,
        limits,
    }))
}

fn required_observation_ref(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<ObservationRef, RuntimeError> {
    let mut object = required_object(arguments, key)?;
    let observation_id = required_opaque_id(&mut object, "observation_id", "obs")?;
    let frame_id = match object.remove("frame_id") {
        Some(Value::Null) | None => None,
        Some(Value::String(value)) => {
            if !is_opaque_id(&value, "frame") {
                return invalid(
                    "argument \"frame_id\" must be null or an opaque frame- followed by 16 lowercase hexadecimal digits",
                );
            }
            Some(value)
        }
        Some(_) => return invalid("argument \"frame_id\" must be null or a string"),
    };
    reject_unknown(object)?;
    Ok(ObservationRef {
        observation_id,
        frame_id,
    })
}

fn is_opaque_id(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 1 + 16
        && value.starts_with(prefix)
        && value.as_bytes().get(prefix.len()) == Some(&b'-')
        && value
            .as_bytes()
            .get(prefix.len() + 1..)
            .is_some_and(|bytes| {
                bytes
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            })
}

fn act_operation(mut object: JsonObject<String, Value>) -> Result<ActOperation, RuntimeError> {
    // A semantic action named as the operation type, with its element, is the
    // semantic operation; its fields are validated exactly as usual.
    if let Some(Value::String(kind)) = object.get("type")
        && matches!(kind.as_str(), "invoke" | "set_value" | "named" | "focus")
        && object.contains_key("element_id")
    {
        if !object.contains_key("action") {
            let mut action = JsonObject::new();
            action.insert("type".into(), Value::String(kind.clone()));
            for key in ["value", "name"] {
                if let Some(value) = object.remove(key) {
                    action.insert(key.into(), value);
                }
            }
            object.insert("action".into(), Value::Object(action));
        }
        object.insert("type".into(), Value::String("semantic".into()));
    }
    let operation = match required_string(&mut object, "type")?.as_str() {
        "pointer" => ActOperation::Pointer {
            action: act_pointer_action(required_object(&mut object, "action")?)?,
        },
        "semantic" => ActOperation::Semantic {
            element_id: required_opaque_id(&mut object, "element_id", "e")?,
            action: element_action(required_object(&mut object, "action")?)?,
        },
        "keyboard" => ActOperation::Keyboard {
            focus: keyboard_focus(&mut object)?,
            events: keyboard_events(&mut object)?,
        },
        "paste" => ActOperation::Paste {
            focus: keyboard_focus(&mut object)?,
            text: paste_text(required_string(&mut object, "text")?)?,
        },
        _ => return invalid("operation.type must be pointer, semantic, keyboard, or paste"),
    };
    reject_unknown(object)?;
    Ok(operation)
}

fn optional_window_action(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<WindowAction, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(WindowAction::default());
    };
    let Value::String(value) = value else {
        return invalid(format!("argument {key:?} must be a string"));
    };
    WindowAction::parse(&value).ok_or_else(|| {
        RuntimeError::invalid_arguments(
            "argument \"action\" must be activate, minimize, maximize, restore, or close",
        )
    })
}

fn paste_text(text: String) -> Result<String, RuntimeError> {
    if text.is_empty() {
        return invalid("paste text must not be empty");
    }
    if text.chars().count() > MAX_TEXT_LIMIT {
        return invalid(format!(
            "paste text must contain at most {MAX_TEXT_LIMIT} Unicode scalar values"
        ));
    }
    if text.contains('\0') {
        return invalid("paste text must not contain NUL");
    }
    Ok(text)
}

fn act_pointer_action(
    mut object: JsonObject<String, Value>,
) -> Result<PointerAction, RuntimeError> {
    let action = match required_string(&mut object, "type")?.as_str() {
        "move" => {
            let (x, y) = coordinate_pair(&mut object, "x", "y")?;
            PointerAction::Move { x, y }
        }
        "click" => {
            let (x, y) = coordinate_pair(&mut object, "x", "y")?;
            PointerAction::Click {
                x,
                y,
                button: optional_button(&mut object, "button")?.unwrap_or(MouseButton::Left),
                count: optional_bounded(&mut object, "count", MAX_CLICK_COUNT)?.unwrap_or(1),
            }
        }
        "drag" => {
            let values = match required(&mut object, "path")? {
                Value::Array(values) => values,
                _ => return invalid("pointer drag path must be an array"),
            };
            if !(2..=MAX_DRAG_POINTS).contains(&values.len()) {
                return invalid(format!(
                    "pointer drag path must contain 2 through {MAX_DRAG_POINTS} points"
                ));
            }
            let path = values
                .into_iter()
                .map(|value| {
                    let mut point = match value {
                        Value::Object(point) => point,
                        _ => {
                            return invalid("pointer drag path points must be objects");
                        }
                    };
                    let result = coordinate_pair(&mut point, "x", "y")?;
                    reject_unknown(point)?;
                    Ok(result)
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?;
            PointerAction::Drag { path }
        }
        "scroll" => {
            let direction = required_string(&mut object, "direction")?;
            let steps = optional_bounded(&mut object, "steps", MAX_SCROLL_STEPS)?.unwrap_or(1);
            let amount = i32::try_from(steps)
                .ok()
                .and_then(|steps| steps.checked_mul(120))
                .ok_or_else(|| RuntimeError::invalid_arguments("scroll steps are too large"))?;
            let (delta_x, delta_y) = match direction.as_str() {
                "up" => (0, -amount),
                "down" => (0, amount),
                "left" => (-amount, 0),
                "right" => (amount, 0),
                _ => return invalid("direction must be up, down, left, or right"),
            };
            let (x, y) = coordinate_pair(&mut object, "x", "y")?;
            PointerAction::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            }
        }
        _ => return invalid("pointer action type must be move, click, drag, or scroll"),
    };
    reject_unknown(object)?;
    Ok(action)
}

/// An omitted `focus` means the target window's current keyboard focus.
fn keyboard_focus(object: &mut JsonObject<String, Value>) -> Result<KeyboardFocus, RuntimeError> {
    if object.contains_key("focus") {
        keyboard_focus_target(required_object(object, "focus")?)
    } else {
        Ok(KeyboardFocus::Window)
    }
}

fn keyboard_focus_target(
    mut object: JsonObject<String, Value>,
) -> Result<KeyboardFocus, RuntimeError> {
    let focus = match required_string(&mut object, "type")?.as_str() {
        "point" => {
            let (x, y) = coordinate_pair(&mut object, "x", "y")?;
            KeyboardFocus::Point(KeyboardPoint { x, y })
        }
        "semantic" => KeyboardFocus::Semantic {
            element_id: required_opaque_id(&mut object, "element_id", "e")?,
        },
        "window" | "current" | "active" => {
            return invalid(
                "omit \"focus\" to type into the target window's current focus; focus.type is semantic or point",
            );
        }
        "element" => {
            return invalid(
                "keyboard focus.type \"element\" does not exist; use {\"type\":\"semantic\",\"element_id\":…}",
            );
        }
        _ => return invalid("keyboard focus.type must be semantic or point"),
    };
    reject_unknown(object)?;
    Ok(focus)
}

fn keyboard_events(
    arguments: &mut JsonObject<String, Value>,
) -> Result<Vec<KeyboardEvent>, RuntimeError> {
    let values = match required(arguments, "events")? {
        Value::Array(values) => values,
        _ => return invalid("keyboard events must be an array"),
    };
    if values.is_empty() || values.len() > MAX_KEYBOARD_EVENTS {
        return invalid(format!(
            "keyboard events must contain 1 through {MAX_KEYBOARD_EVENTS} events"
        ));
    }
    let events: Vec<_> = values
        .into_iter()
        .map(|value| {
            let mut event = match value {
                Value::Object(event) => event,
                _ => return invalid("keyboard events must contain objects"),
            };
            let result = match required_string(&mut event, "type")?.as_str() {
                "press" => {
                    let key = bounded_nonblank(
                        required_string(&mut event, "key")?,
                        "key",
                        MAX_QUERY_LENGTH,
                    )?;
                    if key.matches('+').count() > MAX_KEYBOARD_MODIFIERS {
                        return invalid(format!(
                            "keyboard chords may contain at most {MAX_KEYBOARD_MODIFIERS} modifiers"
                        ));
                    }
                    // Reject unknown key names before any state is consumed.
                    crate::input::keyboard::parse_chord(&key).map_err(|error| {
                        RuntimeError::invalid_arguments(format!(
                            "{error}; use modifiers ctrl/alt/shift/super plus one key such as a, F2, Return, comma or period"
                        ))
                    })?;
                    KeyboardEvent::Press(key)
                }
                "type" => {
                    let text = required_string(&mut event, "text")?;
                    if text.is_empty() {
                        return invalid("keyboard type event text must not be empty");
                    }
                    if text.chars().count() > MAX_KEYBOARD_TRANSACTION_TEXT {
                        return invalid(format!(
                            "keyboard type event text must contain at most {MAX_KEYBOARD_TRANSACTION_TEXT} Unicode scalar values"
                        ));
                    }
                    if text.contains('\0') {
                        return invalid("keyboard type event text must not contain NUL");
                    }
                    KeyboardEvent::Type(text)
                }
                _ => return invalid("keyboard event.type must be press or type"),
            };
            reject_unknown(event)?;
            Ok(result)
        })
        .collect::<Result<_, _>>()?;
    let text_count: usize = events
        .iter()
        .map(|event| match event {
            KeyboardEvent::Type(text) => text.chars().count(),
            KeyboardEvent::Press(_) => 0,
        })
        .sum();
    if text_count > MAX_KEYBOARD_TRANSACTION_TEXT {
        return invalid(format!(
            "keyboard transaction text must contain at most {MAX_KEYBOARD_TRANSACTION_TEXT} Unicode scalar values in total"
        ));
    }
    Ok(events)
}

fn wait_condition(mut object: JsonObject<String, Value>) -> Result<WaitCondition, RuntimeError> {
    let condition = match required_string(&mut object, "type")?.as_str() {
        "human_idle" => WaitCondition::HumanIdle,
        "frame_advanced" => WaitCondition::FrameAdvanced {
            after_frame_id: required_opaque_id(&mut object, "after_frame_id", "frame")?,
        },
        "frame_changed" => WaitCondition::FrameChanged {
            after_frame_id: required_opaque_id(&mut object, "after_frame_id", "frame")?,
        },
        "frame_stable" => WaitCondition::FrameStable {
            for_ms: required_timeout(&mut object, "for_ms", MAX_WAIT_STABLE_MS)?,
        },
        "accessibility_advanced" => WaitCondition::AccessibilityAdvanced {
            after_observation_id: required_opaque_id(&mut object, "after_observation_id", "obs")?,
        },
        "element_state" => WaitCondition::ElementState {
            observation_id: required_opaque_id(&mut object, "observation_id", "obs")?,
            element_id: required_opaque_id(&mut object, "element_id", "e")?,
            state: bounded_nonblank(
                required_string(&mut object, "state")?,
                "state",
                MAX_QUERY_LENGTH,
            )?,
        },
        "element_value" => WaitCondition::ElementValue {
            observation_id: required_opaque_id(&mut object, "observation_id", "obs")?,
            element_id: required_opaque_id(&mut object, "element_id", "e")?,
            value: bounded_text(
                required_string(&mut object, "value")?,
                "value",
                MAX_TEXT_LIMIT,
            )?,
        },
        "window_opened" => WaitCondition::WindowOpened {
            desktop_id: required_window_app_id(&mut object, "desktop_id")?,
        },
        "window_closed" => WaitCondition::WindowClosed {
            window_instance_id: required_opaque_id(&mut object, "window_instance_id", "win")?,
        },
        _ => {
            return invalid(
                "condition.type must be frame_advanced, frame_changed, frame_stable, accessibility_advanced, element_state, element_value, window_opened, window_closed or human_idle; for a plain pause use frame_stable with for_ms",
            );
        }
    };
    reject_unknown(object)?;
    Ok(condition)
}

fn required_timeout(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
    maximum: u64,
) -> Result<u64, RuntimeError> {
    let value = json_integer(&required(arguments, key)?).ok_or_else(|| {
        RuntimeError::invalid_arguments(format!("argument {key:?} must be an integer"))
    })?;
    if value > maximum {
        return invalid(format!(
            "argument {key:?} must be an integer from 0 through {maximum}"
        ));
    }
    Ok(value)
}

fn element_action(mut object: JsonObject<String, Value>) -> Result<ElementAction, RuntimeError> {
    // The rendering advertises `named_actions`, so its singular is a common
    // spelling of the name field.
    if !object.contains_key("name")
        && let Some(name) = object.remove("named_action")
    {
        object.insert("name".into(), name);
    }
    let action = match required_string(&mut object, "type")?.as_str() {
        // invoke with a name is the named action; invoke alone is the primary.
        "invoke" if !object.contains_key("name") => ElementAction::Invoke,
        "invoke" | "named" => {
            let name = bounded_text(
                required_string(&mut object, "name")?,
                "name",
                MAX_QUERY_LENGTH,
            )?;
            if name.trim().is_empty() {
                return invalid("argument \"name\" must not be blank");
            }
            ElementAction::Named(name)
        }
        "focus" => ElementAction::Focus,
        "set_value" => ElementAction::SetValue(bounded_text(
            required_string(&mut object, "value")?,
            "value",
            MAX_TEXT_LIMIT,
        )?),
        _ => return invalid("element action type must be invoke, named, focus, or set_value"),
    };
    reject_unknown(object)?;
    Ok(action)
}

fn required(arguments: &mut JsonObject<String, Value>, key: &str) -> Result<Value, RuntimeError> {
    arguments.remove(key).ok_or_else(|| {
        RuntimeError::invalid_arguments(format!("missing required argument {key:?}"))
    })
}

fn required_object(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<JsonObject<String, Value>, RuntimeError> {
    match required(arguments, key)? {
        Value::Object(object) => Ok(object),
        _ => Err(RuntimeError::invalid_arguments(format!(
            "argument {key:?} must be an object"
        ))),
    }
}

fn required_string(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    match required(arguments, key)? {
        Value::String(value) => Ok(value),
        _ => Err(RuntimeError::invalid_arguments(format!(
            "argument {key:?} must be a string"
        ))),
    }
}

fn bounded_text(value: String, key: &str, maximum: usize) -> Result<String, RuntimeError> {
    if value.chars().count() > maximum {
        return invalid(format!(
            "argument {key:?} must contain at most {maximum} characters"
        ));
    }
    Ok(value)
}

fn bounded_nonblank(value: String, key: &str, maximum: usize) -> Result<String, RuntimeError> {
    let value = bounded_text(value, key, maximum)?;
    let normalized = value.trim();
    if normalized.is_empty() {
        return invalid(format!("argument {key:?} must not be blank"));
    }
    Ok(normalized.to_owned())
}

fn required_desktop_id(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    let value = required_string(arguments, key)?;
    if value.len() == ".desktop".len()
        || !value.ends_with(".desktop")
        || value.chars().any(char::is_whitespace)
    {
        return invalid(format!(
            "argument {key:?} must be an exact non-whitespace desktop ID ending in .desktop"
        ));
    }
    Ok(value)
}

fn required_window_app_id(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    let value = required_string(arguments, key)?;
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return invalid(format!(
            "argument {key:?} must be an exact non-whitespace application ID"
        ));
    }
    Ok(value)
}

fn required_finite(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<f64, RuntimeError> {
    let value = required(arguments, key)?.as_f64().ok_or_else(|| {
        RuntimeError::invalid_arguments(format!("argument {key:?} must be a number"))
    })?;
    if !value.is_finite() {
        return invalid(format!("argument {key:?} must be finite"));
    }
    Ok(value)
}

fn required_coordinate(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<f64, RuntimeError> {
    let value = required_finite(arguments, key)?;
    if value < 0.0 {
        return invalid(format!("argument {key:?} must be non-negative"));
    }
    Ok(value)
}

fn coordinate_pair(
    arguments: &mut JsonObject<String, Value>,
    x: &str,
    y: &str,
) -> Result<(f64, f64), RuntimeError> {
    Ok((
        required_coordinate(arguments, x)?,
        required_coordinate(arguments, y)?,
    ))
}

fn optional_button(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<Option<MouseButton>, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(None);
    };
    match value.as_str() {
        Some("left") => Ok(Some(MouseButton::Left)),
        Some("right") => Ok(Some(MouseButton::Right)),
        Some("middle") => Ok(Some(MouseButton::Middle)),
        _ => invalid(format!("argument {key:?} must be left, right, or middle")),
    }
}

fn optional_bounded<T>(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
    maximum: T,
) -> Result<Option<T>, RuntimeError>
where
    T: Copy + From<u8> + PartialOrd + std::fmt::Display + TryFrom<u64>,
{
    let Some(value) = arguments.remove(key) else {
        return Ok(None);
    };
    let value = json_integer(&value)
        .and_then(|value| T::try_from(value).ok())
        .filter(|value| (T::from(1)..=maximum).contains(value))
        .ok_or_else(|| {
            RuntimeError::invalid_arguments(format!(
                "argument {key:?} must be an integer from 1 through {maximum}"
            ))
        })?;
    Ok(Some(value))
}

fn json_integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let value = value.as_f64()?;
        (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64)
            .then_some(value as u64)
    })
}

fn reject_unknown(arguments: JsonObject<String, Value>) -> Result<(), RuntimeError> {
    if arguments.is_empty() {
        return Ok(());
    }
    let mut keys = arguments
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    invalid(format!("unknown argument(s): {}", keys.join(", ")))
}

fn invalid<T>(message: impl Into<String>) -> Result<T, RuntimeError> {
    Err(RuntimeError::invalid_arguments(message))
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn desktop_selection_is_only_valid_without_a_target_identity() {
        for (name, arguments) in [
            (
                "list_desktop",
                json!({"scope":"windows","desktop":"background"}),
            ),
            (
                "launch_application",
                json!({"desktop_id":"app.desktop","desktop":"background"}),
            ),
            (
                "wait_for",
                json!({"condition":{"type":"window_opened","desktop_id":"app.desktop"},"timeout_ms":100,"desktop":"background"}),
            ),
        ] {
            let RoutedCall {
                desktop,
                arguments: local,
                ..
            } = validate_routed_call(name, arguments.as_object().unwrap().clone()).unwrap();
            assert_eq!(desktop, Some(Desktop::Background));
            assert!(!local.contains_key("desktop"));
            assert!(validate_call(name, local).is_ok());
        }
        for arguments in [
            json!({"scope":"windows","desktop":"Background"}),
            json!({"scope":"windows","desktop":null}),
            json!({"scope":"windows","desktop":0}),
            json!({"scope":"windows","desktop":"background","extra":true}),
        ] {
            assert!(
                validate_routed_call("list_desktop", arguments.as_object().unwrap().clone())
                    .is_err()
            );
        }
        let target = json!({"app_instance_id":"app-0000000000000001","window_instance_id":"win-0000000000000001"});
        for (name, arguments) in [
            (
                "observe",
                json!({"target":target,"view":"both","desktop":"background"}),
            ),
            (
                "wait_for",
                json!({"condition":{"type":"human_idle"},"timeout_ms":120000,"desktop":"background"}),
            ),
            (
                "wait_for",
                json!({"condition":{"type":"human_idle"},"timeout_ms":120000,"desktop":"foreground"}),
            ),
            (
                "wait_for",
                json!({"condition":{"type":"window_closed","window_instance_id":"win-0000000000000001"},"timeout_ms":0,"desktop":"background"}),
            ),
            (
                "wait_for",
                json!({"target":target,"condition":{"type":"window_opened","desktop_id":"app.desktop"},"timeout_ms":0,"desktop":"background"}),
            ),
        ] {
            assert!(validate_routed_call(name, arguments.as_object().unwrap().clone()).is_err());
        }
    }

    #[test]
    fn invalid_compact_action_names_the_real_operations_without_echoing_input() {
        for tool in crate::contract::compact_tool_definitions() {
            let arguments = serde_json::from_value(serde_json::json!({
                "action": "private-unknown-action", "arguments": {}
            }))
            .unwrap();
            let error = validate_mcp_call(tool, arguments).err().unwrap();
            assert_eq!(error.code, "invalid_arguments");
            for action in TOOL_NAMES {
                assert!(error.message.contains(action));
            }
            assert!(error.message.contains("help without action"));
            assert!(!error.message.contains("private-unknown-action"));
        }
    }
}
