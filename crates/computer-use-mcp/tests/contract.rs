use base64::{Engine as _, engine::general_purpose::STANDARD};
use computer_use_mcp::{
    accessibility::{MAX_MODEL_STRUCTURED_BYTES, MAX_MODEL_TEXT_BYTES},
    contract::{SERVER_INSTRUCTIONS, TOOL_NAMES, compact_tool_definitions, tool_definitions},
    encoder::MAX_PNG_BYTES,
    input::keyboard::parse_chord,
    runtime::ToolOutput,
};
use rmcp::model::{ListToolsResult, NumberOrString, ServerJsonRpcMessage, ServerResult};
use serde_json::{Value, json};

const PACKAGED_SKILL: &str = include_str!("../guidance/skill.md");

#[test]
fn tools_list_wire_has_exact_contract_and_stays_within_budget() {
    let tools = tool_definitions();
    let wire = ServerJsonRpcMessage::response(
        ServerResult::ListToolsResult(ListToolsResult::with_all_items(tools.to_vec())),
        NumberOrString::Number(1),
    );
    let serialized = serde_json::to_vec(&wire).expect("serialize complete tools/list");
    println!(
        "tools/list={} bytes; initialize={} bytes; skill={} bytes",
        serialized.len(),
        computer_use_mcp::contract::SERVER_INSTRUCTIONS.len(),
        PACKAGED_SKILL.len()
    );
    assert!(
        serialized.len() <= 13_000,
        "tools/list is {} bytes",
        serialized.len()
    );
    let value = serde_json::to_value(wire).expect("serialize tools/list value");
    let tools = value["result"]["tools"].as_array().expect("wire tools");
    let names: Vec<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(names, TOOL_NAMES);
    let mut total_words = 0;
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_closed_objects(schema);
        assert_required_properties_have_no_defaults(
            schema,
            tool["name"].as_str().expect("tool name"),
        );
        assert!(tool.get("outputSchema").is_none());
        let tool_bytes = serde_json::to_vec(tool).expect("serialize wire tool").len();
        println!("{}={tool_bytes} bytes", tool["name"]);
        assert!(
            tool_bytes <= 7_000,
            "{} is {tool_bytes} bytes",
            tool["name"]
        );
        let words = tool["description"]
            .as_str()
            .expect("tool description")
            .split_whitespace()
            .count();
        assert!(
            words <= 80,
            "{} has {words} description words",
            tool["name"]
        );
        total_words += words;
    }
    assert!(SERVER_INSTRUCTIONS.len() <= 400);
    assert!(
        total_words <= 400,
        "tool descriptions use {total_words} words"
    );
    assert!(PACKAGED_SKILL.len() <= 1_100);
}

#[test]
fn compact_startup_and_on_demand_workflow_have_bounded_context() {
    let wire = ServerJsonRpcMessage::response(
        ServerResult::ListToolsResult(ListToolsResult::with_all_items(
            compact_tool_definitions().to_vec(),
        )),
        NumberOrString::Number(1),
    );
    let compact_bytes = serde_json::to_vec(&wire).unwrap().len();
    let direct_bytes = serde_json::to_vec(&tool_definitions()).unwrap().len();
    assert!(
        compact_bytes <= 1_200,
        "compact tools/list: {compact_bytes}"
    );
    assert!(
        compact_bytes * 10 < direct_bytes,
        "compact tool definitions must save at least 90%"
    );
    // A representative accessibility workflow loads three schemas, each once.
    let workflow_bytes: usize = tool_definitions()
        .iter()
        .filter(|tool| ["list_desktop", "observe", "act"].contains(&tool.name.as_ref()))
        .map(|tool| serde_json::to_vec(&tool).unwrap().len())
        .sum();
    let total = compact_bytes + SERVER_INSTRUCTIONS.len() + PACKAGED_SKILL.len() + workflow_bytes;
    assert!(
        total < direct_bytes,
        "on-demand workflow {total} exceeds eager schemas {direct_bytes}"
    );
    println!(
        "compact tools/list={compact_bytes}; initialize={}; skill={}; list/observe/act schemas={workflow_bytes}; workflow total={total}",
        SERVER_INSTRUCTIONS.len(),
        PACKAGED_SKILL.len(),
    );
}

#[test]
fn complete_tool_result_text_structured_and_image_budgets_are_measurable() {
    let image_base64 = STANDARD.encode(vec![0_u8; MAX_PNG_BYTES]);
    let result = ToolOutput::text("t".repeat(MAX_MODEL_TEXT_BYTES))
        .with_structured_content(json!({
            "payload": "s".repeat(MAX_MODEL_STRUCTURED_BYTES - 32)
        }))
        .with_png_base64(image_base64.clone())
        .into_mcp_result();
    let value = serde_json::to_value(&result).expect("serialize complete call result");
    assert!(value["content"][0]["text"].as_str().unwrap().len() <= MAX_MODEL_TEXT_BYTES);
    assert!(
        serde_json::to_vec(&value["structuredContent"])
            .expect("serialize structured content")
            .len()
            <= MAX_MODEL_STRUCTURED_BYTES
    );
    let max_image_base64 = MAX_PNG_BYTES.div_ceil(3) * 4;
    assert!(value["content"][1]["data"].as_str().unwrap().len() <= max_image_base64);

    let wire = ServerJsonRpcMessage::response(
        ServerResult::CallToolResult(result),
        NumberOrString::Number(3),
    );
    let wire_bytes = serde_json::to_vec(&wire).expect("serialize complete stdio call result");
    assert!(
        wire_bytes.len()
            <= MAX_MODEL_TEXT_BYTES + MAX_MODEL_STRUCTURED_BYTES + max_image_base64 + 4_096,
        "complete call result is {} bytes",
        wire_bytes.len()
    );
}

#[test]
fn inputs_use_exact_opaque_targets_and_bounded_operations() {
    let tools = tool_definitions();
    let schema = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .expect("tool definition")
            .schema_as_json_value()
    };

    let list = schema("list_desktop");
    assert_eq!(list["required"], json!(["scope"]));
    assert_eq!(
        list["properties"]["scope"]["enum"],
        json!(["windows", "applications"])
    );
    assert_eq!(list["properties"]["limit"]["default"], 50);
    assert_eq!(list["properties"]["limit"]["maximum"], 100);
    assert_eq!(list["properties"]["cursor"]["minLength"], 1);
    for name in ["list_desktop", "launch_application", "wait_for"] {
        assert_eq!(
            schema(name)["properties"]["desktop"]["enum"],
            json!(["foreground", "background"])
        );
    }
    for name in ["observe", "act", "activate_window"] {
        assert!(schema(name)["properties"].get("desktop").is_none());
    }
    assert_eq!(
        schema("launch_application")["properties"]["desktop_id"]["pattern"],
        "^[^\\s]+\\.desktop$"
    );

    let activate = schema("activate_window");
    let target = &activate["properties"]["target"];
    assert_eq!(
        target["properties"]["app_instance_id"]["pattern"],
        "^app-[0-9a-f]{16}$"
    );
    assert_eq!(
        target["properties"]["window_instance_id"]["pattern"],
        "^win-[0-9a-f]{16}$"
    );
    assert_eq!(activate["required"], json!(["target"]));
    assert_eq!(
        activate["properties"]["action"]["enum"],
        json!(["activate", "minimize", "maximize", "restore", "close"])
    );
    assert_eq!(activate["properties"]["action"]["default"], "activate");

    let observe = schema("observe");
    assert_eq!(observe["required"], json!(["target", "view"]));
    assert_eq!(
        observe["properties"]["view"]["enum"],
        json!(["screenshot", "accessibility", "both"])
    );
    assert_eq!(
        observe["properties"]["crop"]["enum"],
        json!(["monitor", "target_window"])
    );
    assert_eq!(observe["properties"]["crop"]["default"], "monitor");
    assert_eq!(
        observe["properties"]["accessibility"]["properties"]["limits"]["properties"]["max_nodes"]["maximum"],
        5_000
    );
    assert_eq!(
        observe["properties"]["accessibility"]["properties"]["scope"]["default"],
        "interactive"
    );
    assert_eq!(
        observe["properties"]["accessibility"]["properties"]["limits"]["properties"]["text_limit"]
            ["default"],
        256
    );
    assert_eq!(
        observe["properties"]["accessibility"]["properties"]["limits"]["properties"]["max_nodes"]["default"],
        250
    );

    let act = schema("act");
    assert_eq!(
        discriminants(&act["properties"]["operation"]),
        ["pointer", "semantic", "keyboard", "paste"]
    );
    assert_eq!(
        discriminants(&act["properties"]["operation"]["oneOf"][0]["properties"]["action"]),
        ["move", "click", "drag", "scroll"]
    );
    assert_eq!(
        discriminants(&act["properties"]["operation"]["oneOf"][1]["properties"]["action"]),
        ["invoke", "focus", "named", "set_value"]
    );
    assert_eq!(
        act["properties"]["source_observation"]["properties"]["observation_id"]["pattern"],
        "^obs-[0-9a-f]{16}$"
    );
    assert_eq!(
        act["properties"]["operation"]["oneOf"][0]["properties"]["action"]["oneOf"][2]["properties"]
            ["path"]["maxItems"],
        32
    );
    assert_eq!(
        act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"][0]["maxItems"],
        8
    );
    let key_schema = &act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"]
        [0]["items"]["properties"]["key"];
    let key_pattern = key_schema["pattern"].as_str().expect("key pattern");
    assert_eq!(
        key_pattern,
        r"^\s*[^\s+](?:[^+]*[^\s+])?(?:\s*\+\s*[^\s+](?:[^+]*[^\s+])?){0,4}\s*$"
    );
    assert_eq!(key_schema["maxLength"], 1_000);
    for (key, expected) in [
        ("F1", true),
        (" Ctrl + Alt + Shift + Super + F1 ", true),
        ("Ctrl+Alt+Shift+Super+Meta+F1", false),
        ("Ctrl++F1", false),
        ("A+B+C+D+E+F1", false),
    ] {
        assert_eq!(schema_key_shape(key), expected, "schema shape for {key:?}");
        assert_eq!(
            parse_chord(key).is_ok(),
            expected,
            "runtime shape for {key:?}"
        );
    }
    assert_eq!(
        act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"][1]["maxItems"],
        1
    );
    assert_eq!(
        act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"][1]["items"]["properties"]
            ["text"]["maxLength"],
        4_096
    );
    assert_eq!(
        act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"][1]["items"]["properties"]
            ["text"]["minLength"],
        1
    );
    assert_eq!(
        act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"][1]["items"]["properties"]
            ["text"]["pattern"],
        "^[^\\u0000]+$"
    );
    let keyboard_focus = &act["properties"]["operation"]["oneOf"][2]["properties"]["focus"];
    assert_eq!(
        keyboard_focus["oneOf"][0]["properties"]["type"]["const"],
        "point"
    );
    assert_eq!(
        keyboard_focus["oneOf"][1]["properties"]["type"]["const"],
        "semantic"
    );
    assert_eq!(
        keyboard_focus["oneOf"][1]["properties"]["element_id"]["pattern"],
        "^e-[0-9a-f]{16}$"
    );
    assert_eq!(
        keyboard_focus["oneOf"][1]["required"],
        json!(["type", "element_id"])
    );
    let paste = &act["properties"]["operation"]["oneOf"][3];
    assert_eq!(paste["properties"]["type"]["const"], "paste");
    assert_eq!(
        paste["properties"]["focus"]["oneOf"][0]["properties"]["type"]["const"],
        "point"
    );
    assert_eq!(
        paste["properties"]["focus"]["oneOf"][1]["properties"]["type"]["const"],
        "semantic"
    );
    assert_eq!(paste["properties"]["text"]["minLength"], 1);
    assert_eq!(paste["properties"]["text"]["maxLength"], 100_000);
    assert_eq!(paste["properties"]["text"]["pattern"], "^[^\\u0000]+$");
    assert_eq!(paste["required"], json!(["type", "focus", "text"]));
    let wait = schema("wait_for");
    assert_eq!(
        discriminants(&wait["properties"]["condition"]),
        [
            "frame_advanced",
            "frame_changed",
            "frame_stable",
            "accessibility_advanced",
            "element_state",
            "element_value",
            "window_opened",
            "window_closed",
        ]
    );
    assert_eq!(wait["required"], json!(["condition", "timeout_ms"]));
    assert!(wait["properties"]["timeout_ms"].get("default").is_none());
    assert_eq!(wait["properties"]["timeout_ms"]["maximum"], 5_000);
    let frame_stable = &wait["properties"]["condition"]["oneOf"][2];
    assert_eq!(frame_stable["properties"]["type"]["const"], "frame_stable");
    assert_eq!(frame_stable["required"], json!(["type", "for_ms"]));
    assert!(
        frame_stable["properties"]["for_ms"]
            .get("default")
            .is_none()
    );
    assert_eq!(
        wait["properties"]["condition"]["oneOf"][1]["properties"]["type"]["const"],
        "frame_changed"
    );
    let window_opened = &wait["properties"]["condition"]["oneOf"][6];
    assert_eq!(
        window_opened["properties"]["type"]["const"],
        "window_opened"
    );
    assert_eq!(
        window_opened["properties"]["desktop_id"]["pattern"],
        "^[^\\s]+$"
    );
    assert_eq!(window_opened["required"], json!(["type", "desktop_id"]));
    let window_closed = &wait["properties"]["condition"]["oneOf"][7];
    assert_eq!(
        window_closed["properties"]["type"]["const"],
        "window_closed"
    );
    assert_eq!(
        window_closed["properties"]["window_instance_id"]["pattern"],
        "^win-[0-9a-f]{16}$"
    );
    assert_eq!(
        window_closed["required"],
        json!(["type", "window_instance_id"])
    );

    for (name, bound, expected) in [
        ("cursor", &list["properties"]["cursor"]["maxLength"], 128),
        (
            "query",
            &observe["properties"]["accessibility"]["properties"]["query"]["maxLength"],
            1_000,
        ),
        (
            "text_limit",
            &observe["properties"]["accessibility"]["properties"]["limits"]["properties"]["text_limit"]
                ["anyOf"][0]["maximum"],
            100_000,
        ),
        (
            "max_nodes",
            &observe["properties"]["accessibility"]["properties"]["limits"]["properties"]["max_nodes"]
                ["maximum"],
            5_000,
        ),
        (
            "max_depth",
            &observe["properties"]["accessibility"]["properties"]["limits"]["properties"]["max_depth"]
                ["maximum"],
            128,
        ),
        (
            "click count",
            &act["properties"]["operation"]["oneOf"][0]["properties"]["action"]["oneOf"][1]["properties"]
                ["count"]["maximum"],
            3,
        ),
        (
            "drag points",
            &act["properties"]["operation"]["oneOf"][0]["properties"]["action"]["oneOf"][2]["properties"]
                ["path"]["maxItems"],
            32,
        ),
        (
            "scroll steps",
            &act["properties"]["operation"]["oneOf"][0]["properties"]["action"]["oneOf"][3]["properties"]
                ["steps"]["maximum"],
            100,
        ),
        (
            "named",
            &act["properties"]["operation"]["oneOf"][1]["properties"]["action"]["oneOf"][2]["properties"]
                ["name"]["maxLength"],
            1_000,
        ),
        (
            "set_value",
            &act["properties"]["operation"]["oneOf"][1]["properties"]["action"]["oneOf"][3]["properties"]
                ["value"]["maxLength"],
            100_000,
        ),
        (
            "keyboard text",
            &act["properties"]["operation"]["oneOf"][2]["properties"]["events"]["oneOf"][1]["items"]
                ["properties"]["text"]["maxLength"],
            4_096,
        ),
        (
            "wait timeout",
            &wait["properties"]["timeout_ms"]["maximum"],
            5_000,
        ),
        (
            "wait stability",
            &wait["properties"]["condition"]["oneOf"][2]["properties"]["for_ms"]["maximum"],
            1_500,
        ),
        (
            "element state",
            &wait["properties"]["condition"]["oneOf"][4]["properties"]["state"]["maxLength"],
            1_000,
        ),
        (
            "element value",
            &wait["properties"]["condition"]["oneOf"][5]["properties"]["value"]["maxLength"],
            100_000,
        ),
    ] {
        assert_eq!(bound, expected, "{name}");
    }
}

#[test]
fn annotations_match_tool_side_effects() {
    let read_only = ["list_desktop", "observe", "wait_for"];
    for tool in tool_definitions() {
        let annotations = serde_json::to_value(&tool.annotations).expect("serialize annotations");
        let expected_read_only = read_only.contains(&tool.name.as_ref());
        assert_eq!(annotations["openWorldHint"], true, "{}", tool.name);
        assert_eq!(
            annotations["readOnlyHint"], expected_read_only,
            "{}",
            tool.name
        );
        assert_eq!(
            annotations["idempotentHint"], expected_read_only,
            "{}",
            tool.name
        );
        assert_eq!(
            annotations["destructiveHint"],
            matches!(tool.name.as_ref(), "activate_window" | "act"),
            "{}",
            tool.name
        );
    }
}

fn assert_closed_objects(value: &Value) {
    match value {
        Value::Object(object) => {
            if object.get("type") == Some(&Value::String("object".into())) {
                assert_eq!(
                    object.get("additionalProperties"),
                    Some(&Value::Bool(false))
                );
            }
            for value in object.values() {
                assert_closed_objects(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                assert_closed_objects(value);
            }
        }
        _ => {}
    }
}

fn assert_required_properties_have_no_defaults(value: &Value, path: &str) {
    match value {
        Value::Object(object) => {
            if let (Some(properties), Some(required)) = (
                object.get("properties").and_then(Value::as_object),
                object.get("required").and_then(Value::as_array),
            ) {
                for name in required {
                    let name = name.as_str().expect("required property name");
                    let property = properties
                        .get(name)
                        .unwrap_or_else(|| panic!("{path} requires absent property {name:?}"));
                    assert!(
                        property.get("default").is_none(),
                        "{path}.{name} is required but advertises a default"
                    );
                }
            }
            for (name, child) in object {
                assert_required_properties_have_no_defaults(child, &format!("{path}.{name}"));
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                assert_required_properties_have_no_defaults(child, &format!("{path}[{index}]"));
            }
        }
        _ => {}
    }
}

fn schema_key_shape(value: &str) -> bool {
    let tokens = value.split('+').map(str::trim).collect::<Vec<_>>();
    !tokens.is_empty() && tokens.len() <= 5 && tokens.iter().all(|token| !token.is_empty())
}

fn discriminants(schema: &Value) -> Vec<&str> {
    schema["oneOf"]
        .as_array()
        .expect("discriminated union")
        .iter()
        .map(|variant| {
            variant["properties"]["type"]["const"]
                .as_str()
                .expect("type discriminator")
        })
        .collect()
}
