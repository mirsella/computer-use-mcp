use std::sync::Arc;

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::json;
use serde_json::{Map as JsonObject, Value};

use crate::validation::{
    AccessibilityScope, DEFAULT_ACCESSIBILITY_MAX_DEPTH, DEFAULT_ACCESSIBILITY_MAX_NODES,
    DEFAULT_ACCESSIBILITY_TEXT_LIMIT, DEFAULT_DESKTOP_PAGE_SIZE, MAX_CLICK_COUNT,
    MAX_DESKTOP_PAGE_SIZE, MAX_DRAG_POINTS, MAX_KEYBOARD_EVENTS, MAX_KEYBOARD_MODIFIERS,
    MAX_KEYBOARD_TRANSACTION_TEXT, MAX_QUERY_LENGTH, MAX_SCROLL_STEPS, MAX_TEXT_LIMIT,
    MAX_TREE_DEPTH, MAX_TREE_NODES, MAX_WAIT_STABLE_MS, MAX_WAIT_TIMEOUT_MS,
};

pub const TOOL_NAMES: [&str; 6] = [
    "list_desktop",
    "launch_application",
    "activate_window",
    "observe",
    "act",
    "wait_for",
];

pub const SERVER_INSTRUCTIONS: &str = r#"Operate only on evidence returned by this server. First call list_desktop with scope "windows" and copy one complete target exactly; use scope "applications" only for an exact desktop_id. Never invent or normalize IDs or substitute a title, PID, selector, or guessed geometry. Launch is an acknowledgement only: list windows again, then observe the new target before acting.

For observe choose "screenshot", "accessibility", or "both". Copy opaque target, observation_id, frame_id, element_id, cursors, and advertised capabilities exactly; do not reuse stale evidence. Activation returns its documented request/active evidence and replacement_observation when present, but does not prove seat focus. Act names its exact source_observation (a frame is required for spatial input); use its replacement observation when present, otherwise observe before another mutation. A stale target requires list_desktop again; a stale observation or element requires observe again.

Spatial coordinates and keyboard focus points are exact half-open pixels (0 <= x < width, 0 <= y < height) in the complete selected-monitor PNG. AT-SPI bounds are diagnostic and are never convertible to PNG coordinates. Never use Alt+Tab or another window-switch shortcut. Keyboard requires visibly intended point focus and either press-only or one type-only transaction; separate routing, text, and submit across replacement observations. Prefer semantic set_value when advertised.

Interpret outcomes carefully: not_started means dispatch did not begin and retry only after recovery; unknown or completed means the action may have happened, so observe or list current state and never retry blindly. Dispatch, synchronization, or flush proves neither seat focus, application/text delivery, nor effect. Restart or re-enable only when recovery says the portal/session is exhausted; arbitrary failures are not restart proof. Screenshots are complete selected-monitor images and may expose unrelated windows or private content; send them only to a trusted host."#;

pub fn tool_definitions() -> Vec<Tool> {
    vec![
        tool(
            "list_desktop",
            "List exact running window targets or installed application IDs. Call windows first when choosing a target; copy returned opaque targets and cursors exactly and do not reuse stale cursors.",
            object(
                json!({
                    "scope": {"type": "string", "enum": ["windows", "applications"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_DESKTOP_PAGE_SIZE, "default": DEFAULT_DESKTOP_PAGE_SIZE},
                    "cursor": {"type": "string", "minLength": 1, "maxLength": 128}
                }),
                &["scope"],
            ),
            true,
            false,
        ),
        tool(
            "launch_application",
            "Launch the exact `.desktop` ID returned by list_desktop with scope applications. This is an acknowledgement only, not a mapped window; list windows and observe before acting.",
            object(
                json!({
                    "desktop_id": {"type": "string", "pattern": "^[^\\s]+\\.desktop$"}
                }),
                &["desktop_id"],
            ),
            false,
            false,
        ),
        tool(
            "activate_window",
            "Request activation for an exact target copied from list_desktop and report available activation and replacement_observation evidence. It does not prove seat focus or application delivery; observe before any follow-up mutation.",
            object(json!({"target": target_schema()}), &["target"]),
            false,
            true,
        ),
        tool(
            "observe",
            "Choose screenshot, accessibility, or both for an exact target. Copy returned observation, frame, and element IDs and advertised capabilities; use this fresh evidence for act or wait_for and never use stale IDs.",
            object(
                json!({
                    "target": target_schema(),
                    "view": {"type": "string", "enum": ["screenshot", "accessibility", "both"]},
                    "accessibility": accessibility_request_schema()
                }),
                &["target", "view"],
            ),
            true,
            false,
        ),
        tool(
            "act",
            "Act once with semantic, pointer, or point-focused keyboard input. Every keyboard phase clicks its focus point again: recompute it from that phase's fresh replacement PNG; never reuse a prior point after the UI moves. Prefer advertised semantic set_value. Spatial input requires source-frame pixels. Use its replacement observation, or observe before another mutation.",
            act_input_schema(),
            false,
            true,
        ),
        tool(
            "wait_for",
            "Wait for bounded frame or accessibility evidence using exact IDs from prior results. frame_stable requires for_ms; timeout_ms max 5000. A timeout is not proof that nothing changed; use returned evidence or observe again before acting.",
            object(
                json!({
                    "target": target_schema(),
                    "condition": wait_condition_schema(),
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_TIMEOUT_MS}
                }),
                &["target", "condition", "timeout_ms"],
            ),
            true,
            false,
        ),
    ]
}

fn tool(
    name: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
    destructive: bool,
) -> Tool {
    let annotations = ToolAnnotations::new()
        .read_only(read_only)
        .open_world(true)
        .destructive(destructive)
        .idempotent(read_only);
    Tool::new(name, description, Arc::new(into_object(schema))).annotate(annotations)
}

fn target_schema() -> Value {
    object(
        json!({
            "app_instance_id": {"type": "string", "pattern": "^app-[0-9a-f]{16}$"},
            "window_instance_id": {"type": "string", "pattern": "^win-[0-9a-f]{16}$"}
        }),
        &["app_instance_id", "window_instance_id"],
    )
}

fn input_observation_ref_schema() -> Value {
    object(
        json!({
            "observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$", "description": "Copy unchanged from observe."},
            "frame_id": {"type": ["string", "null"], "pattern": "^frame-[0-9a-f]{16}$", "description": "Exact ready PNG frame; required for spatial input."}
        }),
        &["observation_id"],
    )
}

fn input_observation_ref_with_frame_schema() -> Value {
    object(
        json!({
            "observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$", "description": "Copy unchanged from observe."},
            "frame_id": {"type": "string", "pattern": "^frame-[0-9a-f]{16}$", "description": "Exact ready PNG frame required for spatial input."}
        }),
        &["observation_id", "frame_id"],
    )
}

fn act_input_schema() -> Value {
    let mut schema = into_object(object(
        json!({
            "target": target_schema(),
            "source_observation": input_observation_ref_schema(),
            "operation": act_operation_schema()
        }),
        &["target", "source_observation", "operation"],
    ));
    schema.insert(
        "allOf".into(),
        json!([{
            "if": {
                "properties": {
                    "operation": {
                        "properties": {"type": {"enum": ["pointer", "keyboard"]}}
                    }
                }
            },
            "then": {
                "properties": {"source_observation": input_observation_ref_with_frame_schema()}
            }
        }]),
    );
    Value::Object(schema)
}

fn accessibility_request_schema() -> Value {
    let scopes = AccessibilityScope::ALL
        .into_iter()
        .map(AccessibilityScope::as_str)
        .collect::<Vec<_>>();
    object(
        json!({
            "scope": {"type": "string", "enum": scopes, "default": AccessibilityScope::default().as_str()},
            "query": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH},
            "limits": object(json!({
                "text_limit": {"anyOf": [
                    {"type": "integer", "minimum": 0, "maximum": MAX_TEXT_LIMIT},
                    {"const": "max"}
                ], "default": DEFAULT_ACCESSIBILITY_TEXT_LIMIT},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": MAX_TREE_NODES, "default": DEFAULT_ACCESSIBILITY_MAX_NODES},
                "max_depth": {"type": "integer", "minimum": 1, "maximum": MAX_TREE_DEPTH, "default": DEFAULT_ACCESSIBILITY_MAX_DEPTH}
            }), &[])
        }),
        &[],
    )
}

fn act_operation_schema() -> Value {
    let key_chord_pattern = key_chord_pattern();
    json!({
        "description": "Choose one operation advertised by the source observation.",
        "oneOf": [
        object(json!({
            "type": {"const": "pointer"},
            "action": {"oneOf": [
                action_object("move", json!({"x": coordinate_schema(), "y": coordinate_schema()}), &["x", "y"]),
                action_object("click", json!({
                    "x": coordinate_schema(), "y": coordinate_schema(),
                    "button": {"type": "string", "enum": ["left", "right", "middle"], "default": "left"},
                    "count": {"type": "integer", "minimum": 1, "maximum": MAX_CLICK_COUNT, "default": 1}
                }), &["x", "y"]),
                action_object("drag", json!({"path": {"type": "array", "minItems": 2, "maxItems": MAX_DRAG_POINTS, "items": object(json!({"x": coordinate_schema(), "y": coordinate_schema()}), &["x", "y"])} }), &["path"]),
                action_object("scroll", json!({
                    "x": coordinate_schema(), "y": coordinate_schema(),
                    "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
                    "steps": {"type": "integer", "minimum": 1, "maximum": MAX_SCROLL_STEPS, "default": 1}
                }), &["x", "y", "direction"])
            ]}
        }), &["type", "action"]),
        object(json!({
            "type": {"const": "semantic"},
            "element_id": {"type": "string", "pattern": "^e-[0-9a-f]{16}$"},
            "action": semantic_action_schema()
        }), &["type", "element_id", "action"]),
        object(json!({
             "type": {"const": "keyboard"},
              "focus": object_with_description(json!({"type": {"const": "point"}, "x": coordinate_schema(), "y": coordinate_schema()}), &["type", "x", "y"], "Visible point in the exact source PNG; AT-SPI focus is not keyboard authority."),
              "events": {"description": "Press-only or one type-only event; separate routing, text, and submit.", "oneOf": [
                 {"type": "array", "minItems": 1, "maxItems": MAX_KEYBOARD_EVENTS, "items": action_object("press", json!({"key": {"type": "string", "pattern": key_chord_pattern, "maxLength": MAX_QUERY_LENGTH, "description": "At most four '+' modifiers and key; whitespace around tokens is trimmed."}}), &["key"])},
                 {"type": "array", "minItems": 1, "maxItems": 1, "items": action_object("type", json!({"text": {"type": "string", "minLength": 1, "maxLength": MAX_KEYBOARD_TRANSACTION_TEXT, "pattern": "^[^\\u0000]+$", "description": "One non-empty text event."}}), &["text"])}
             ]}
        }), &["type", "focus", "events"])
    ]})
}

fn semantic_action_schema() -> Value {
    json!({"oneOf": [
        action_object("invoke", json!({}), &[]),
        action_object("focus", json!({}), &[]),
        action_object("named", json!({"name": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH}}), &["name"]),
        action_object("set_value", json!({"value": {"type": "string", "maxLength": MAX_TEXT_LIMIT}}), &["value"])
    ]})
}

fn wait_condition_schema() -> Value {
    json!({
        "description": "Use an exact prior frame or observation ID. A timeout is bounded evidence, not proof that nothing changed.",
        "oneOf": [
        action_object("frame_advanced", json!({"after_frame_id": {"type": "string", "pattern": "^frame-[0-9a-f]{16}$"}}), &["after_frame_id"]),
        action_object("frame_changed", json!({"after_frame_id": {"type": "string", "pattern": "^frame-[0-9a-f]{16}$"}}), &["after_frame_id"]),
        action_object("frame_stable", json!({"for_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_STABLE_MS}}), &["for_ms"]),
        action_object("accessibility_advanced", json!({"after_observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$"}}), &["after_observation_id"]),
        action_object("element_state", json!({
            "observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$"},
            "element_id": {"type": "string", "pattern": "^e-[0-9a-f]{16}$"},
            "state": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH}
        }), &["observation_id", "element_id", "state"]),
        action_object("element_value", json!({
            "observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$"},
            "element_id": {"type": "string", "pattern": "^e-[0-9a-f]{16}$"},
            "value": {"type": "string", "maxLength": MAX_TEXT_LIMIT}
        }), &["observation_id", "element_id", "value"])
    ]})
}

fn coordinate_schema() -> Value {
    json!({"type": "number", "minimum": 0, "description": "PNG half-open coordinate from the exact source observation frame."})
}

fn key_chord_pattern() -> String {
    format!(
        r"^\s*[^\s+](?:[^+]*[^\s+])?(?:\s*\+\s*[^\s+](?:[^+]*[^\s+])?){{0,{MAX_KEYBOARD_MODIFIERS}}}\s*$"
    )
}

fn action_object(kind: &str, properties: Value, required: &[&str]) -> Value {
    let mut properties = into_object(properties);
    properties.insert("type".into(), json!({"const": kind}));
    let required = std::iter::once("type")
        .chain(required.iter().copied())
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": into_object(properties),
        "required": required,
        "additionalProperties": false
    })
}

fn object_with_description(properties: Value, required: &[&str], description: &str) -> Value {
    let mut object = into_object(object(properties, required));
    object.insert("description".into(), json!(description));
    Value::Object(object)
}

fn into_object(value: Value) -> JsonObject<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => panic!("schema helper requires an object"),
    }
}
