use std::collections::BTreeSet;

use serde_json::{Map as JsonObject, Value};

use crate::errors::RuntimeError;

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
pub const MAX_KEYBOARD_EXPANDED_ACTIONS: usize = 4_096;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 5_000;
// The stable interval must fit inside the default wait deadline while still
// leaving room to acquire at least one fresh frame and observe its metadata.
pub const MAX_WAIT_STABLE_MS: u64 = 1_500;
pub const DEFAULT_DESKTOP_PAGE_SIZE: usize = 50;
pub const MAX_DESKTOP_PAGE_SIZE: usize = 100;
pub const DEFAULT_ACCESSIBILITY_TEXT_LIMIT: usize = 256;
pub const DEFAULT_ACCESSIBILITY_MAX_NODES: usize = 250;
pub const DEFAULT_ACCESSIBILITY_MAX_DEPTH: usize = 64;

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetRef {
    pub app_instance_id: String,
    pub window_instance_id: String,
}

impl TargetRef {
    pub fn as_json(&self) -> Value {
        serde_json::json!({
            "app_instance_id": self.app_instance_id,
            "window_instance_id": self.window_instance_id,
        })
    }
}

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
    pub limits: AccessibilityLimits,
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
pub enum KeyboardEvent {
    Press(String),
    Type(String),
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
        focus: KeyboardPoint,
        events: Vec<KeyboardEvent>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
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
                ActOperation::Pointer { .. } | ActOperation::Keyboard { .. } => true,
                ActOperation::Semantic { .. } => false,
            },
            Self::WaitFor { condition, .. } => match condition {
                WaitCondition::FrameAdvanced { .. }
                | WaitCondition::FrameChanged { .. }
                | WaitCondition::FrameStable { .. } => true,
                WaitCondition::AccessibilityAdvanced { .. }
                | WaitCondition::ElementState { .. }
                | WaitCondition::ElementValue { .. } => false,
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
    },
    Observe {
        target: TargetRef,
        view: ObserveView,
        accessibility: Option<AccessibilityRequest>,
    },
    Act {
        target: TargetRef,
        source: ObservationRef,
        operation: ActOperation,
    },
    WaitFor {
        target: TargetRef,
        condition: WaitCondition,
        timeout_ms: u64,
    },
}

pub fn validate_call(
    name: &str,
    mut arguments: JsonObject<String, Value>,
) -> Result<ToolCall, RuntimeError> {
    let call = match name {
        "list_desktop" => {
            let scope = match required_string(&mut arguments, "scope")?.as_str() {
                "windows" => DesktopScope::Windows,
                "applications" => DesktopScope::Applications,
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
        },
        "observe" => ToolCall::Observe {
            target: required_target(&mut arguments, "target")?,
            view: required_observe_view(&mut arguments, "view")?,
            accessibility: optional_accessibility(&mut arguments, "accessibility")?,
        },
        "act" => ToolCall::Act {
            target: required_target(&mut arguments, "target")?,
            source: required_observation_ref(&mut arguments, "source_observation")?,
            operation: act_operation(required_object(&mut arguments, "operation")?)?,
        },
        "wait_for" => ToolCall::WaitFor {
            target: required_target(&mut arguments, "target")?,
            condition: wait_condition(required_object(&mut arguments, "condition")?)?,
            timeout_ms: required_timeout(&mut arguments, "timeout_ms", MAX_WAIT_TIMEOUT_MS)?,
        },
        _ => return invalid(format!("unknown tool {name:?}")),
    };
    reject_unknown(arguments)?;
    call.validate_policy()?;
    validate_action_source(&call)?;
    Ok(call)
}

fn validate_action_source(call: &ToolCall) -> Result<(), RuntimeError> {
    let ToolCall::Act {
        source,
        operation: ActOperation::Pointer { .. } | ActOperation::Keyboard { .. },
        ..
    } = call
    else {
        return Ok(());
    };
    if source.frame_id.is_none() {
        return invalid(
            "source_observation.frame_id is required for pointer and keyboard operations",
        );
    }
    Ok(())
}

fn required_target(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<TargetRef, RuntimeError> {
    let mut object = required_object(arguments, key)?;
    let target = TargetRef {
        app_instance_id: required_opaque_id(&mut object, "app_instance_id", "app")?,
        window_instance_id: required_opaque_id(&mut object, "window_instance_id", "win")?,
    };
    reject_unknown(object)?;
    Ok(target)
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
    let operation = match required_string(&mut object, "type")?.as_str() {
        "pointer" => ActOperation::Pointer {
            action: act_pointer_action(required_object(&mut object, "action")?)?,
        },
        "semantic" => ActOperation::Semantic {
            element_id: required_opaque_id(&mut object, "element_id", "e")?,
            action: element_action(required_object(&mut object, "action")?)?,
        },
        "keyboard" => ActOperation::Keyboard {
            focus: keyboard_focus_target(required_object(&mut object, "focus")?)?,
            events: keyboard_events(&mut object)?,
        },
        _ => return invalid("operation.type must be pointer, semantic, or keyboard"),
    };
    reject_unknown(object)?;
    Ok(operation)
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
            let values = required(&mut object, "path")?
                .as_array()
                .cloned()
                .ok_or_else(|| {
                    RuntimeError::invalid_arguments("pointer drag path must be an array")
                })?;
            if !(2..=MAX_DRAG_POINTS).contains(&values.len()) {
                return invalid(format!(
                    "pointer drag path must contain 2 through {MAX_DRAG_POINTS} points"
                ));
            }
            let path = values
                .into_iter()
                .map(|value| {
                    let mut point = value.as_object().cloned().ok_or_else(|| {
                        RuntimeError::invalid_arguments("pointer drag path points must be objects")
                    })?;
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

fn keyboard_focus_target(
    mut object: JsonObject<String, Value>,
) -> Result<KeyboardPoint, RuntimeError> {
    let focus = match required_string(&mut object, "type")?.as_str() {
        "point" => {
            let (x, y) = coordinate_pair(&mut object, "x", "y")?;
            KeyboardPoint { x, y }
        }
        "element" => return invalid("keyboard focus must be a point in the source screenshot PNG"),
        _ => return invalid("keyboard focus.type must be point"),
    };
    reject_unknown(object)?;
    Ok(focus)
}

fn keyboard_events(
    arguments: &mut JsonObject<String, Value>,
) -> Result<Vec<KeyboardEvent>, RuntimeError> {
    let values = required(arguments, "events")?
        .as_array()
        .cloned()
        .ok_or_else(|| RuntimeError::invalid_arguments("keyboard events must be an array"))?;
    if values.is_empty() || values.len() > MAX_KEYBOARD_EVENTS {
        return invalid(format!(
            "keyboard events must contain 1 through {MAX_KEYBOARD_EVENTS} events"
        ));
    }
    let kinds = values
        .iter()
        .map(|value| {
            value
                .as_object()
                .and_then(|object| object.get("type"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    let type_count = kinds.iter().filter(|kind| **kind == Some("type")).count();
    let press_count = kinds.iter().filter(|kind| **kind == Some("press")).count();
    if type_count > 0 && (type_count != 1 || press_count != 0 || values.len() != 1) {
        return invalid(
            "keyboard events must be either press-only or exactly one non-empty type event; mixed press/type transactions are rejected",
        );
    }
    if type_count == 0 && press_count != values.len() {
        return invalid("keyboard event.type must be press or type");
    }
    values
        .into_iter()
        .map(|value| {
            let mut event = value.as_object().cloned().ok_or_else(|| {
                RuntimeError::invalid_arguments("keyboard events must contain objects")
            })?;
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
        .collect()
}

fn wait_condition(mut object: JsonObject<String, Value>) -> Result<WaitCondition, RuntimeError> {
    let condition = match required_string(&mut object, "type")?.as_str() {
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
        _ => return invalid("condition.type is not a supported wait condition"),
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
    let action = match required_string(&mut object, "type")?.as_str() {
        "invoke" => ElementAction::Invoke,
        "named" => {
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
    required(arguments, key)?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            RuntimeError::invalid_arguments(format!("argument {key:?} must be an object"))
        })
}

fn required_string(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    required(arguments, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            RuntimeError::invalid_arguments(format!("argument {key:?} must be a string"))
        })
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
    let keys = arguments.keys().cloned().collect::<BTreeSet<_>>();
    invalid(format!(
        "unknown argument(s): {}",
        keys.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

fn invalid<T>(message: impl Into<String>) -> Result<T, RuntimeError> {
    Err(RuntimeError::invalid_arguments(message))
}
