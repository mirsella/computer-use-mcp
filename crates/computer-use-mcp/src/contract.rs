use std::sync::{Arc, LazyLock};

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::json;
use serde_json::{Map as JsonObject, Value};

use crate::validation::{
    AccessibilityScope, DEFAULT_ACCESSIBILITY_MAX_DEPTH, DEFAULT_ACCESSIBILITY_MAX_NODES,
    DEFAULT_ACCESSIBILITY_TEXT_LIMIT, DEFAULT_DESKTOP_PAGE_SIZE, MAX_CLICK_COUNT,
    MAX_DESKTOP_PAGE_SIZE, MAX_DRAG_POINTS, MAX_HUMAN_IDLE_TIMEOUT_MS, MAX_KEYBOARD_EVENTS,
    MAX_KEYBOARD_MODIFIERS, MAX_KEYBOARD_TRANSACTION_TEXT, MAX_QUERY_LENGTH, MAX_SCROLL_STEPS,
    MAX_TEXT_LIMIT, MAX_TREE_DEPTH, MAX_TREE_NODES, MAX_WAIT_STABLE_MS, MAX_WAIT_TIMEOUT_MS,
    WindowAction,
};

pub const TOOL_NAMES: [&str; 6] = [
    "list_desktop",
    "launch_application",
    "activate_window",
    "observe",
    "act",
    "wait_for",
];

pub const SERVER_INSTRUCTIONS: &str = "Copy IDs unchanged. Live elements bind semantic source/target. Points need source/frame. Use replacements. Stale: reobserve. not_started: recover; unknown/completed: inspect, never replay. Dispatch != app effect. HumanInputBusy: human_idle then observe. Background only if requested. Modal-opening menus can block invoke: prefer screenshot clicks or advertised keybindings.";

pub fn compact_tool_definitions() -> &'static [Tool] {
    &*COMPACT_TOOLS
}

pub fn tool_definitions() -> &'static [Tool] {
    &*DIRECT_TOOLS
}

pub fn tool_definition(name: &str) -> Option<&'static Tool> {
    tool_definitions().iter().find(|tool| tool.name == name)
}

/// Compact argument reference returned by `help {"action": ...}`: the
/// operation's description followed by copyable templates. Each line of the
/// form `<action> {json}` is one complete valid call with APP/WIN/OBS/FRAME/EL
/// placeholders. `tests/contract.rs` validates every such line and requires
/// every schema field, enum value, and numeric maximum to be named, so this
/// stays the same contract as the JSON Schemas at a fraction of their size.
pub fn action_reference(name: &str) -> Option<String> {
    let templates = match name {
        "list_desktop" => {
            r#"list_desktop {"scope":"windows","desktop":"foreground","limit":50}
scope windows (default) | applications. desktop foreground (default) | background. limit 1-100. Pass the returned "cursor" for the next page."#
        }
        "launch_application" => {
            r#"launch_application {"desktop_id":"org.kde.kwrite.desktop","desktop":"foreground"}
desktop_id comes from list_desktop scope=applications. desktop foreground (default) | background."#
        }
        "activate_window" => {
            r#"activate_window {"target":{"app_instance_id":"APP","window_instance_id":"WIN"},"action":"activate"}
action activate (default) | minimize | maximize | restore | close."#
        }
        "observe" => {
            r#"observe {"target":{"app_instance_id":"APP","window_instance_id":"WIN"},"view":"both","crop":"target_window","accessibility":{"scope":"interactive","limits":{"max_nodes":40,"max_depth":64,"text_limit":256}}}
observe {"target":{"app_instance_id":"APP","window_instance_id":"WIN"},"view":"accessibility","accessibility":{"search":{"name":"Save","role":"button","exact":false,"limit":20,"max_nodes":500,"timeout_ms":2000}}}
view screenshot | accessibility | both. crop monitor (default) | target_window. scope interactive (default) | visible | full. Limits: max_nodes 1-5000 (default 250), max_depth 1-128, text_limit 0-100000 or "max".
search walks the application for name and/or role (limit 1-100, timeout_ms 1-2000). "query":"text" instead filters the collected nodes; never send both."#
        }
        "act" => {
            r#"act {"operation":{"type":"semantic","element_id":"EL","action":{"type":"invoke"}}}
Other semantic actions: {"type":"set_value","value":"text"} | {"type":"named","name":"ADVERTISED ACTION"} | {"type":"focus"}.
act {"source_observation":{"observation_id":"OBS","frame_id":"FRAME"},"operation":{"type":"pointer","action":{"type":"click","x":120,"y":80,"button":"left","count":1}}}
x,y are pixels of that frame's PNG. button left | right | middle. count 1-3. Other pointer actions: {"type":"move","x":120,"y":80} | {"type":"scroll","x":120,"y":80,"direction":"down","steps":3} (up | down | left | right, steps 1-100) | {"type":"drag","path":[{"x":120,"y":80},{"x":300,"y":80}]} (2-32 points).
act {"operation":{"type":"keyboard","focus":{"type":"semantic","element_id":"EL"},"events":[{"type":"press","key":"ctrl+a"},{"type":"type","text":"hello"},{"type":"press","key":"Return"}]}}
1-8 events. key: ctrl/alt/shift/super modifiers plus one key (a, F2, Return, Tab, Escape, Down, comma).
act {"operation":{"type":"paste","focus":{"type":"semantic","element_id":"EL"},"text":"long or multi-line text"}}
act {"target":{"app_instance_id":"APP","window_instance_id":"WIN"},"operation":{"type":"keyboard","events":[{"type":"press","key":"ctrl+s"}]}}
No focus: keys/paste go to the active window's current focus (shortcuts, typing after a click); "target" optional: last observed window.
Pixel focus: "focus":{"type":"point","x":120,"y":80}, source_observation with frame_id."#
        }
        "wait_for" => {
            r#"wait_for {"target":{"app_instance_id":"APP","window_instance_id":"WIN"},"condition":{"type":"frame_stable","for_ms":300},"timeout_ms":2000}
wait_for {"target":{"app_instance_id":"APP","window_instance_id":"WIN"},"condition":{"type":"element_value","observation_id":"OBS","element_id":"EL","value":"Done"},"timeout_ms":5000}
wait_for {"condition":{"type":"window_opened","desktop_id":"org.kde.kwrite.desktop"},"desktop":"foreground","timeout_ms":5000}
Other conditions: {"type":"frame_changed","after_frame_id":"FRAME"} | {"type":"frame_advanced","after_frame_id":"FRAME"} | {"type":"accessibility_advanced","after_observation_id":"OBS"} | {"type":"element_state","observation_id":"OBS","element_id":"EL","state":"checked"} | {"type":"window_closed","window_instance_id":"WIN"} | {"type":"human_idle"}.
for_ms 0-1500. timeout_ms 0-5000; human_idle takes no target and up to 120000. desktop foreground | background applies to window_opened only."#
        }
        _ => return None,
    };
    let description = tool_definition(name)?.description.as_deref()?;
    Some(format!("{description}\n{templates}"))
}

static COMPACT_TOOLS: LazyLock<[Tool; 2]> = LazyLock::new(|| {
    let action = json!({
        "type": "string",
        "enum": TOOL_NAMES
    });
    [
        tool(
            "help",
            "List six native actions, or fetch one action's reference. Use for fields beyond dispatch's common templates, including pointer/keyboard input and waits.",
            object(json!({"action": action}), &[]),
            true,
            false,
        ),
        tool(
            "dispatch",
            "Copy APP/WIN/EL exactly. Live EL binds source/target for semantics and semantic-focus keyboard/paste.\nlist_desktop {}\nobserve {\"target\":{\"app_instance_id\":\"APP\",\"window_instance_id\":\"WIN\"},\"view\":\"accessibility\",\"accessibility\":{\"limits\":{\"max_nodes\":40}}}\nact {\"operation\":{\"type\":\"semantic\",\"element_id\":\"EL\",\"action\":{\"type\":\"invoke\"}}}\nKeep element_id inside operation. set_value action={\"type\":\"set_value\",\"value\":\"8\"}: numbers are strings. Prefer advertised invoke/set_value; no capability means use screenshot. Tabs need parent tab-list keys. EL IDs persist; act readbacks list only changes. unknown: inspect, never replay. Screenshot: observe view=\"screenshot\", no screenshot field. Waits need condition+timeout_ms, not duration_ms. help(action) for other fields.",
            object(
                json!({
                    "action": action,
                    "arguments": {"type": "object", "additionalProperties": true}
                }),
                &["action", "arguments"],
            ),
            false,
            true,
        ),
    ]
});

static DIRECT_TOOLS: LazyLock<[Tool; 6]> = LazyLock::new(|| {
    [
        tool(
            "list_desktop",
            "Find windows or installed app IDs on foreground (default) or private background. Targets route later calls. Paginate with returned cursor.",
            object(
                json!({
                    "desktop": desktop_schema(),
                    "scope": {"type": "string", "enum": ["windows", "applications"], "default": "windows"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_DESKTOP_PAGE_SIZE, "default": DEFAULT_DESKTOP_PAGE_SIZE},
                    "cursor": {"type": "string", "minLength": 1, "maxLength": 128}
                }),
                &[],
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
            "Activate, minimize, maximize, restore, or close. Use instead of Alt+Tab. Non-activation needs KDE capability. Activation may switch desktops; it does not prove keyboard focus.",
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
            "Inspect a target. Point input uses its PNG frame, never AT-SPI bounds. target_window needs verified KDE geometry or returns monitor pixels with a reason. accessibility.search bounds native name/role search; query only filters collected nodes.",
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
            "Apply one action using latest IDs. An exact live semantic element or semantic keyboard/paste focus binds source/target; either may be omitted, supplied authority must match. Point input requires source and frame_id. Returned readback is fresh. Keyboard: 1–8 ordered press/type events, total text ≤4096 scalars; preflight before emission. Semantic focus verifies without pointer motion. Paste never restores prior clipboard.",
            act_input_schema(),
            false,
            true,
        ),
        tool(
            "wait_for",
            "Wait, then observe. human_idle: targetless foreground, 60s quiet, timeout<=120000; others<=5000. Frame/element waits need target. window_opened matches existing compositor app IDs; window_closed needs listed ID.",
            wait_input_schema(),
            true,
            false,
        ),
    ]
});

fn tool(
    name: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
    destructive: bool,
) -> Tool {
    let annotations = ToolAnnotations::new()
        // First AT-SPI use may enable the session bridge (SECURITY.md); that
        // service activation leaves no user-visible state, keeping this true.
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
            "observation_id": {"type": "string", "pattern": "^obs-[0-9a-f]{16}$"},
            "frame_id": {"type": ["string", "null"], "pattern": "^frame-[0-9a-f]{16}$", "description": "Exact PNG frame for pointer/point focus; omit for semantic focus."}
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
        &["operation"],
    ));
    let frame_required = json!({"required": ["source_observation"], "properties": {"source_observation": {
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
    let mut search = object(
        json!({
            "name": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH},
            "role": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH},
            "exact": {"type": "boolean", "default": false},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
            "max_nodes": {"type": "integer", "minimum": 1, "maximum": MAX_TREE_NODES, "default": 500},
            "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 2000}
        }),
        &[],
    );
    search["anyOf"] = json!([{"required": ["name"]}, {"required": ["role"]}]);
    let mut schema = object(
        json!({
            "scope": {"type": "string", "enum": scopes, "default": AccessibilityScope::default().as_str()},
            "query": {"type": "string", "pattern": ".*\\S.*", "maxLength": MAX_QUERY_LENGTH},
            "search": search,
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
    );
    schema["not"] = json!({"required": ["query", "search"]});
    schema
}

fn act_operation_schema() -> Value {
    let key_chord_pattern = key_chord_pattern();
    json!({
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
               "events": {"type": "array", "minItems": 1, "maxItems": MAX_KEYBOARD_EVENTS, "items": {"oneOf": [
                  action_object("press", json!({"key": {"type": "string", "pattern": key_chord_pattern, "maxLength": MAX_QUERY_LENGTH}}), &["key"]),
                  action_object("type", json!({"text": {"type": "string", "minLength": 1, "maxLength": MAX_KEYBOARD_TRANSACTION_TEXT, "pattern": "^[^\\u0000]+$"}}), &["text"])
              ]}}
         }), &["type", "events"]),
        object(json!({
             "type": {"const": "paste"},
              "focus": keyboard_focus_schema(),
               "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT_LIMIT, "pattern": "^[^\\u0000]+$"}
        }), &["type", "text"])
    ]})
}

/// Focus for keyboard and paste operations: either a visible point in the
/// exact source PNG (focus-clicked first) or an opaque AT-SPI element ID
/// (grabbed and verified first, typed with no pointer movement and no
/// screenshot mapping). Both variants are closed objects. Omitting `focus`
/// types into the active target window's current focus instead.
fn keyboard_focus_schema() -> Value {
    json!({
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
            "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_HUMAN_IDLE_TIMEOUT_MS}
        }),
        &["condition", "timeout_ms"],
    ));
    schema.insert("allOf".into(), json!([{
        "if": {"properties": {"condition": {"properties": {"type": {"enum": ["window_opened", "window_closed", "human_idle"]}}}}},
        "else": {"required": ["target"]}
    }, {
        "if": {"required":["desktop"]},
        "then": {"not":{"required":["target"]},"properties":{"condition":{"properties":{"type":{"const":"window_opened"}}}}}
    }, {
        "if": {"properties":{"condition":{"properties":{"type":{"const":"human_idle"}}}}},
        "then": {"not":{"required":["target"]}},
        "else": {"properties":{"timeout_ms":{"maximum":MAX_WAIT_TIMEOUT_MS}}}
    }]));
    Value::Object(schema)
}

fn wait_condition_schema() -> Value {
    json!({
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
        }), &["window_instance_id"]),
        action_object("human_idle", json!({}), &[])
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
