use std::collections::BTreeSet;

use serde_json::{Map as JsonObject, Value};

use crate::errors::RuntimeError;

pub const MAX_CLICK_COUNT: usize = 3;
pub const MAX_SCROLL_STEPS: u32 = 100;
pub const MAX_TEXT_LIMIT: usize = 100_000;
pub const MAX_TREE_NODES: usize = 5_000;
pub const MAX_TREE_DEPTH: usize = 128;
pub const MAX_ELEMENT_ID: usize = MAX_TREE_NODES - 1;
pub const MAX_QUERY_LENGTH: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationScope {
    Running,
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextLimit {
    Count(usize),
    Max,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ObservationView {
    #[default]
    Full,
    Visible,
    Interactive,
}

impl ObservationView {
    pub const ALL: [Self; 3] = [Self::Full, Self::Visible, Self::Interactive];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Visible => "visible",
            Self::Interactive => "interactive",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.as_str() == value)
    }
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
        from: (f64, f64),
        to: (f64, f64),
    },
    Scroll {
        x: f64,
        y: f64,
        delta_x: i32,
        delta_y: i32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardAction {
    Press(String),
    Type(String),
}

impl ToolCall {
    pub(crate) fn validate_policy(&self) -> Result<(), RuntimeError> {
        let Self::Keyboard {
            action: KeyboardAction::Press(key),
            ..
        } = self
        else {
            return Ok(());
        };
        let mut parts = key.split('+').map(str::trim);
        let has_alt = parts.clone().any(|part| part.eq_ignore_ascii_case("alt"));
        let has_tab = parts.any(|part| part.eq_ignore_ascii_case("tab"));
        if has_alt && has_tab {
            return Err(RuntimeError::unsupported_desktop_focus_switch());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    Point((f64, f64)),
    Element(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    ListApplications {
        scope: ApplicationScope,
    },
    LaunchApplication {
        desktop_id: String,
    },
    Observe {
        target: String,
        view: ObservationView,
        query: Option<String>,
        text_limit: Option<TextLimit>,
        max_tree_nodes: Option<usize>,
        max_tree_depth: Option<usize>,
    },
    ActOnElement {
        state_id: String,
        element_id: String,
        action: ElementAction,
    },
    Pointer {
        state_id: String,
        action: PointerAction,
    },
    Keyboard {
        state_id: String,
        focus: KeyboardFocus,
        action: KeyboardAction,
    },
}

pub fn validate_call(
    name: &str,
    mut arguments: JsonObject<String, Value>,
) -> Result<ToolCall, RuntimeError> {
    let call = match name {
        "list_applications" => {
            let scope = match required_string(&mut arguments, "scope")?.as_str() {
                "running" => ApplicationScope::Running,
                "installed" => ApplicationScope::Installed,
                _ => return invalid("scope must be \"running\" or \"installed\""),
            };
            ToolCall::ListApplications { scope }
        }
        "launch_application" => ToolCall::LaunchApplication {
            desktop_id: required_desktop_id(&mut arguments, "desktop_id")?,
        },
        "observe" => ToolCall::Observe {
            target: required_nonblank(&mut arguments, "target")?,
            view: optional_observation_view(&mut arguments, "view")?,
            query: optional_query(&mut arguments, "query")?,
            text_limit: optional_text_limit(&mut arguments, "text_limit")?,
            max_tree_nodes: optional_bounded(&mut arguments, "max_tree_nodes", MAX_TREE_NODES)?,
            max_tree_depth: optional_bounded(&mut arguments, "max_tree_depth", MAX_TREE_DEPTH)?,
        },
        "act_on_element" => ToolCall::ActOnElement {
            state_id: required_state_id(&mut arguments, "state_id")?,
            element_id: required_element_id(&mut arguments, "element_id")?,
            action: element_action(required_object(&mut arguments, "action")?)?,
        },
        "pointer" => ToolCall::Pointer {
            state_id: required_state_id(&mut arguments, "state_id")?,
            action: pointer_action(required_object(&mut arguments, "action")?)?,
        },
        "keyboard" => ToolCall::Keyboard {
            state_id: required_state_id(&mut arguments, "state_id")?,
            focus: keyboard_focus(required_object(&mut arguments, "focus")?)?,
            action: keyboard_action(required_object(&mut arguments, "action")?)?,
        },
        _ => return invalid(format!("unknown tool {name:?}")),
    };
    reject_unknown(arguments)?;
    call.validate_policy()?;
    Ok(call)
}

fn element_action(mut object: JsonObject<String, Value>) -> Result<ElementAction, RuntimeError> {
    let action = match required_string(&mut object, "type")?.as_str() {
        "invoke" => ElementAction::Invoke,
        "named" => {
            let name = required_string(&mut object, "name")?;
            if name.trim().is_empty() {
                return invalid("argument \"name\" must not be blank");
            }
            ElementAction::Named(name)
        }
        "focus" => ElementAction::Focus,
        "set_value" => ElementAction::SetValue(required_string(&mut object, "value")?),
        _ => return invalid("element action type must be invoke, named, focus, or set_value"),
    };
    reject_unknown(object)?;
    Ok(action)
}

fn pointer_action(mut object: JsonObject<String, Value>) -> Result<PointerAction, RuntimeError> {
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
        "drag" => PointerAction::Drag {
            from: coordinate_pair(&mut object, "from_x", "from_y")?,
            to: coordinate_pair(&mut object, "to_x", "to_y")?,
        },
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

fn keyboard_action(mut object: JsonObject<String, Value>) -> Result<KeyboardAction, RuntimeError> {
    let action = match required_string(&mut object, "type")?.as_str() {
        "press" => KeyboardAction::Press(required_nonblank(&mut object, "key")?),
        "type" => KeyboardAction::Type(required_string(&mut object, "text")?),
        _ => return invalid("keyboard action type must be press or type"),
    };
    reject_unknown(object)?;
    Ok(action)
}

fn keyboard_focus(mut object: JsonObject<String, Value>) -> Result<KeyboardFocus, RuntimeError> {
    let focus = if object.contains_key("element_id") {
        KeyboardFocus::Element(required_element_id(&mut object, "element_id")?)
    } else {
        KeyboardFocus::Point(coordinate_pair(&mut object, "x", "y")?)
    };
    reject_unknown(object)?;
    Ok(focus)
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

fn required_nonblank(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    let value = required_string(arguments, key)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return invalid(format!("argument {key:?} must not be blank"));
    }
    Ok(trimmed.to_owned())
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

fn required_state_id(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    let value = required_string(arguments, key)?;
    let valid = value.len() == 18
        && value.starts_with("s-")
        && value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return invalid(format!(
            "argument {key:?} must match s- followed by 16 lowercase hexadecimal digits"
        ));
    }
    Ok(value)
}

fn required_element_id(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<String, RuntimeError> {
    match required(arguments, key)? {
        Value::String(value)
            if value.len() <= 4
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && value
                    .parse::<usize>()
                    .is_ok_and(|value| value <= MAX_ELEMENT_ID) =>
        {
            Ok(value)
        }
        Value::Number(value) => {
            let parsed = value.as_u64().or_else(|| {
                value.as_f64().and_then(|value| {
                    (value.is_finite()
                        && value >= 0.0
                        && value.fract() == 0.0
                        && value <= MAX_ELEMENT_ID as f64)
                        .then_some(value as u64)
                })
            });
            match parsed.and_then(|value| usize::try_from(value).ok()) {
                Some(value) if value <= MAX_ELEMENT_ID => Ok(value.to_string()),
                _ => invalid(format!(
                    "argument {key:?} must be an element ID from 0 through {MAX_ELEMENT_ID}"
                )),
            }
        }
        _ => invalid(format!(
            "argument {key:?} must be an element ID from 0 through {MAX_ELEMENT_ID}"
        )),
    }
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

fn optional_observation_view(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<ObservationView, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(ObservationView::default());
    };
    value
        .as_str()
        .and_then(ObservationView::parse)
        .ok_or_else(|| {
            RuntimeError::invalid_arguments(format!(
                "argument {key:?} must be full, visible, or interactive"
            ))
        })
}

fn optional_query(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<Option<String>, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(None);
    };
    let query = value.as_str().ok_or_else(|| {
        RuntimeError::invalid_arguments(format!("argument {key:?} must be a string"))
    })?;
    if query.chars().count() > MAX_QUERY_LENGTH {
        return invalid(format!(
            "argument {key:?} must contain at most {MAX_QUERY_LENGTH} characters"
        ));
    }
    let query = query.trim();
    if query.is_empty() {
        return invalid(format!("argument {key:?} must not be blank"));
    }
    Ok(Some(query.to_owned()))
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

fn optional_text_limit(
    arguments: &mut JsonObject<String, Value>,
    key: &str,
) -> Result<Option<TextLimit>, RuntimeError> {
    let Some(value) = arguments.remove(key) else {
        return Ok(None);
    };
    if value.as_str() == Some("max") {
        return Ok(Some(TextLimit::Max));
    }
    let count = json_integer(&value)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            RuntimeError::invalid_arguments(format!(
                "argument {key:?} must be an integer or \"max\""
            ))
        })?;
    if count > MAX_TEXT_LIMIT {
        return invalid(format!("argument {key:?} must not exceed {MAX_TEXT_LIMIT}"));
    }
    Ok(Some(TextLimit::Count(count)))
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
