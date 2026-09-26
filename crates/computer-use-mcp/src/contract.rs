use std::sync::Arc;

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::json;
use serde_json::{Map as JsonObject, Value};

use crate::validation::{
    AccessibilityScope, DEFAULT_ACCESSIBILITY_MAX_DEPTH, DEFAULT_ACCESSIBILITY_MAX_NODES,
    DEFAULT_ACCESSIBILITY_TEXT_LIMIT, DEFAULT_DESKTOP_PAGE_SIZE, MAX_CLICK_COUNT,
    MAX_DESKTOP_PAGE_SIZE, MAX_DRAG_POINTS, MAX_KEYBOARD_EVENTS, MAX_KEYBOARD_MODIFIERS,
    MAX_KEYBOARD_TRANSACTION_TEXT, MAX_QUERY_LENGTH, MAX_SCROLL_STEPS, MAX_TEXT_LIMIT,
    MAX_TREE_DEPTH, MAX_TREE_NODES, MAX_WAIT_STABLE_MS, MAX_WAIT_TIMEOUT_MS, WindowAction,
};

pub const TOOL_NAMES: [&str; 6] = [
    "list_desktop",
    "launch_application",
    "activate_window",
    "observe",
    "act",
    "wait_for",
];

pub const SERVER_INSTRUCTIONS: &str = "Copy returned IDs unchanged. Stale target: rediscover; stale observation or element: observe again. not_started permits recovery then retry; unknown/completed requires inspecting state first. Dispatch does not prove application effect. UserTakeoverInterrupted requires stopping, user authorization, and an MCP restart.";

pub fn compact_tool_definitions() -> Vec<Tool> {
    vec![
        tool(
            "help",
            "Discover desktop operations, or fetch one action's schema. Covers windows, app launch, screenshots, accessibility, input, and waits on foreground or private background desktops.",
            object(json!({"action": {"type": "string", "minLength": 1}}), &[]),
            true,
            false,
        ),
        tool(
            "dispatch",
            "Execute a desktop action. Use help when its schema is not in context; put action-specific fields inside arguments.",
            object(
                json!({
                    "action": {"type": "string", "minLength": 1},
                    "arguments": {"type": "object", "additionalProperties": true}
                }),
                &["action", "arguments"],
            ),
            false,
            true,
        ),
    ]
}

pub fn tool_definitions() -> Vec<Tool> {
    vec![
        tool(
            "list_desktop",
            "Find windows or installed application IDs on foreground (default) or a lazily started private background desktop. Targets route subsequent calls automatically. Paginate with the returned cursor.",
            object(
                json!({
                    "desktop": desktop_schema(),
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
            "Launch an installed desktop_id on foreground (default) or background. Acknowledges the launch request, not window readiness.",
            object(
                json!({
                    "desktop": desktop_schema(),
                    "desktop_id": {"type": "string", "pattern": "^[^\\s]+\\.desktop$"}
                }),
                &["desktop_id"],
            ),
            false,
            false,
        ),
        tool(
            "activate_window",
            "Activate, minimize, maximize, restore, or close a target. Use this instead of Alt+Tab. Non-activation actions require KDE window-management capability. Activation may switch virtual desktops and does not prove keyboard focus.",
            object(
                json!({
                    "target": target_schema(),
                    "action": {"type": "string", "enum": window_action_names(), "default": WindowAction::default().as_str()}
                }),
                &["target"],
            ),
            false,
            true,
        ),
        tool(
            "observe",
            "Inspect a target. target_window crops with verified KDE geometry, otherwise returns the monitor image with a reason. Coordinates are pixels in the returned PNG, within its dimensions, never AT-SPI bounds.",
            object(
                json!({
                    "target": target_schema(),
                    "view": {"type": "string", "enum": ["screenshot", "accessibility", "both"]},
                    "crop": {"type": "string", "enum": ["monitor", "target_window"], "default": "monitor"},
                    "accessibility": accessibility_request_schema()
                }),
                &["target", "view"],
            ),
            true,
            false,
        ),
        tool(
            "act",
            "Apply one action to a source observation. Point focus clicks again on every keyboard/paste call; semantic focus grabs the element without pointer motion and requires verified focus. Paste uses the session clipboard when granted, otherwise simulated typing; it clears its clipboard transfer without restoring prior contents.",
            act_input_schema(),
            false,
            true,
        ),
        tool(
            "wait_for",
            "Wait for evidence up to timeout_ms. Frame/element conditions require target. window_opened accepts desktop (default foreground) and matches existing windows too. window_closed routes by a listed window ID. App-ID matching requires compositor metadata. Timeout does not prove unchanged state; observe before visual input.",
            wait_input_schema(),
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

fn desktop_schema() -> Value {
    json!({"type":"string","enum":["foreground","background"]})
}

fn input_observation_ref_schema() -> Value {
    object(
        json!({
            "observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$", "description": "Copy unchanged from observe."},
            "frame_id": {"type": ["string", "null"], "pattern": "^frame-[0-9a-f]{16}$", "description": "Exact ready PNG frame; required for pointer and point-focus input, omit for semantic focus."}
        }),
        &["observation_id"],
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
    let frame_required = json!({"properties": {"source_observation": {
        "required": ["frame_id"], "properties": {"frame_id": {"type": "string"}}
    }}});
    schema.insert(
        "allOf".into(),
        json!([
        {
            "if": {
                "properties": {
                    "operation": {
                        "properties": {"type": {"enum": ["pointer"]}}
                    }
                }
            },
            "then": frame_required
        },
        {
            // Point-focus typing needs the exact source frame for its focus
            // click; semantic-focus typing verifies AT-SPI focus instead and
            // takes no frame_id.
            "if": {
                "properties": {
                    "operation": {
                        "properties": {
                            "type": {"enum": ["keyboard", "paste"]},
                            "focus": {"properties": {"type": {"const": "point"}}}
                        }
                    }
                }
            },
            "then": frame_required
        }
        ]),
    );
    Value::Object(schema)
}

fn accessibility_request_schema() -> Value {
    let scopes = AccessibilityScope::ALL.map(|scope| scope.as_str());
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
              "focus": keyboard_focus_schema(),
              "events": {"description": "Press-only or one type-only event; separate routing, text, and submit.", "oneOf": [
                 {"type": "array", "minItems": 1, "maxItems": MAX_KEYBOARD_EVENTS, "items": action_object("press", json!({"key": {"type": "string", "pattern": key_chord_pattern, "maxLength": MAX_QUERY_LENGTH, "description": "At most four '+' modifiers and key; whitespace around tokens is trimmed."}}), &["key"])},
                 {"type": "array", "minItems": 1, "maxItems": 1, "items": action_object("type", json!({"text": {"type": "string", "minLength": 1, "maxLength": MAX_KEYBOARD_TRANSACTION_TEXT, "pattern": "^[^\\u0000]+$", "description": "One non-empty text event."}}), &["text"])}
              ]}
         }), &["type", "focus", "events"]),
        object(json!({
             "type": {"const": "paste"},
              "focus": keyboard_focus_schema(),
               "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT_LIMIT, "pattern": "^[^\\u0000]+$"}
        }), &["type", "focus", "text"])
    ]})
}

/// Focus for keyboard and paste operations: either a visible point in the
/// exact source PNG (focus-clicked first) or an opaque AT-SPI element ID
/// (grabbed and verified first, typed with no pointer movement and no
/// screenshot mapping). Both variants are closed objects.
fn keyboard_focus_schema() -> Value {
    json!({
        "description": "Point clicks once; semantic grabs element focus without pointer motion.",
        "oneOf": [
            object(json!({"type": {"const": "point"}, "x": coordinate_schema(), "y": coordinate_schema()}), &["type", "x", "y"]),
            object(json!({
                "type": {"const": "semantic"},
                "element_id": {"type": "string", "pattern": "^e-[0-9a-f]{16}$"}
            }), &["type", "element_id"])
        ]
    })
}

fn semantic_action_schema() -> Value {
    json!({"oneOf": [
        action_object("invoke", json!({}), &[]),
        action_object("focus", json!({}), &[]),
        action_object("named", json!({"name": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH}}), &["name"]),
        action_object("set_value", json!({"value": {"type": "string", "maxLength": MAX_TEXT_LIMIT}}), &["value"])
    ]})
}

fn wait_input_schema() -> Value {
    let mut schema = into_object(object(
        json!({
            "desktop": desktop_schema(),
            "target": target_schema(),
            "condition": wait_condition_schema(),
            "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_TIMEOUT_MS}
        }),
        &["condition", "timeout_ms"],
    ));
    schema.insert("allOf".into(), json!([{
        "if": {"properties": {"condition": {"properties": {"type": {"enum": ["window_opened", "window_closed"]}}}}},
        "else": {"required": ["target"]}
    }, {
        "if": {"required":["desktop"]},
        "then": {"not":{"required":["target"]},"properties":{"condition":{"properties":{"type":{"const":"window_opened"}}}}}
    }]));
    Value::Object(schema)
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
        }), &["observation_id", "element_id", "value"]),
        action_object("window_opened", json!({
            "desktop_id": {"type": "string", "pattern": "^[^\\s]+$", "description": "Exact installed desktop ID, with or without the .desktop suffix."}
        }), &["desktop_id"]),
        action_object("window_closed", json!({
            "window_instance_id": {"type": "string", "pattern": "^win-[0-9a-f]{16}$"}
        }), &["window_instance_id"])
    ]})
}

fn window_action_names() -> [&'static str; 5] {
    WindowAction::ALL.map(|action| action.as_str())
}

fn coordinate_schema() -> Value {
    json!({"type": "number", "minimum": 0})
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

fn into_object(value: Value) -> JsonObject<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => panic!("schema helper requires an object"),
    }
}
