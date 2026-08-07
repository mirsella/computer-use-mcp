use std::collections::BTreeSet;

use computer_use_mcp::{
    contract::{TOOL_NAMES, tool_definitions},
    runtime::ToolOutput,
};
use serde_json::{Value, json};

#[test]
fn tools_have_exact_order_and_schemas() {
    let tools = tool_definitions();
    let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, TOOL_NAMES);

    let schema = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .expect("tool definition")
            .schema_as_json_value()
    };
    assert_object(&schema("list_applications"), &["scope"], &["scope"]);
    assert_eq!(
        schema("list_applications")["properties"]["scope"]["enum"],
        json!(["running", "installed"])
    );
    assert_object(
        &schema("launch_application"),
        &["desktop_id"],
        &["desktop_id"],
    );
    assert_eq!(
        schema("launch_application")["properties"]["desktop_id"]["pattern"],
        "^[^\\s]+\\.desktop$"
    );

    let observe = schema("observe");
    assert_object(
        &observe,
        &[
            "target",
            "view",
            "query",
            "text_limit",
            "max_tree_nodes",
            "max_tree_depth",
        ],
        &["target"],
    );
    assert_eq!(
        observe["properties"]["view"]["enum"],
        json!(["full", "visible", "interactive"])
    );
    assert_eq!(observe["properties"]["query"]["pattern"], ".*\\S.*");
    assert_eq!(observe["properties"]["query"]["maxLength"], 1_000);
    assert_eq!(observe["properties"]["max_tree_nodes"]["maximum"], 5_000);
    assert_eq!(observe["properties"]["max_tree_depth"]["maximum"], 128);
    assert_eq!(
        observe["properties"]["text_limit"]["anyOf"][0]["maximum"],
        100_000
    );

    let act = schema("act_on_element");
    assert_object(
        &act,
        &["state_id", "element_id", "action"],
        &["state_id", "element_id", "action"],
    );
    assert_eq!(
        act["properties"]["element_id"]["anyOf"][1]["maximum"],
        4_999
    );
    assert_eq!(act["properties"]["state_id"], state_id());
    assert_union(
        &act["properties"]["action"],
        &[
            ("invoke", &["type"], &["type"]),
            ("focus", &["type"], &["type"]),
            ("named", &["type", "name"], &["type", "name"]),
            ("set_value", &["type", "value"], &["type", "value"]),
        ],
    );

    let pointer = schema("pointer");
    assert_object(&pointer, &["state_id", "action"], &["state_id", "action"]);
    assert_eq!(pointer["properties"]["state_id"], state_id());
    assert_union(
        &pointer["properties"]["action"],
        &[
            ("move", &["type", "x", "y"], &["type", "x", "y"]),
            (
                "click",
                &["type", "x", "y", "button", "count"],
                &["type", "x", "y"],
            ),
            (
                "drag",
                &["type", "from_x", "from_y", "to_x", "to_y"],
                &["type", "from_x", "from_y", "to_x", "to_y"],
            ),
            (
                "scroll",
                &["type", "x", "y", "direction", "steps"],
                &["type", "x", "y", "direction"],
            ),
        ],
    );
    assert_eq!(
        pointer["properties"]["action"]["oneOf"][1]["properties"]["count"]["maximum"],
        3
    );
    assert_eq!(
        pointer["properties"]["action"]["oneOf"][1]["properties"]["button"]["default"],
        "left"
    );
    assert_eq!(
        pointer["properties"]["action"]["oneOf"][1]["properties"]["button"]["enum"],
        json!(["left", "right", "middle"])
    );
    assert_eq!(
        pointer["properties"]["action"]["oneOf"][3]["properties"]["steps"]["maximum"],
        100
    );
    assert_eq!(
        pointer["properties"]["action"]["oneOf"][3]["properties"]["steps"]["default"],
        1
    );
    assert_eq!(
        pointer["properties"]["action"]["oneOf"][3]["properties"]["direction"]["enum"],
        json!(["up", "down", "left", "right"])
    );
    let keyboard = schema("keyboard");
    assert_object(
        &keyboard,
        &["state_id", "focus", "action"],
        &["state_id", "focus", "action"],
    );
    let focus = keyboard["properties"]["focus"]["oneOf"].as_array().unwrap();
    assert_eq!(focus.len(), 2);
    assert_object(&focus[0], &["x", "y"], &["x", "y"]);
    assert_object(&focus[1], &["element_id"], &["element_id"]);
    assert_eq!(focus[0]["properties"]["x"]["minimum"], 0);
    assert_eq!(
        focus[1]["properties"]["element_id"]["anyOf"][1]["maximum"],
        4_999
    );
    assert_union(
        &keyboard["properties"]["action"],
        &[
            ("press", &["type", "key"], &["type", "key"]),
            ("type", &["type", "text"], &["type", "text"]),
        ],
    );
}

#[test]
fn annotations_match_the_inherited_contract() {
    let tools = tool_definitions();
    for tool in tools {
        let annotations = serde_json::to_value(tool.annotations).expect("serialize annotations");
        let expected = if matches!(tool.name.as_ref(), "list_applications" | "observe") {
            json!({
                "openWorldHint": true,
                "readOnlyHint": true,
            })
        } else if tool.name.as_ref() == "launch_application" {
            json!({
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true,
                "readOnlyHint": false,
            })
        } else {
            json!({
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true,
                "readOnlyHint": false,
            })
        };
        assert_eq!(annotations, expected, "{} annotations", tool.name);
    }
}

#[test]
fn output_schemas_have_expected_shapes() {
    let tools = tool_definitions();
    let output_schema = |name: &str| {
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .expect("tool definition");
        Value::Object(
            tool.output_schema
                .as_ref()
                .expect("output schema")
                .as_ref()
                .clone(),
        )
    };

    let list_schema = output_schema("list_applications");
    let list = &list_schema["oneOf"][0];
    assert_object(list, &["scope", "applications"], &["scope", "applications"]);
    assert_eq!(list["properties"]["scope"]["type"], "string");
    assert_eq!(list["properties"]["applications"]["type"], "array");

    let launch_schema = output_schema("launch_application");
    let launch = &launch_schema["oneOf"][0];
    assert_object(
        launch,
        &["status", "desktop_id", "name"],
        &["status", "desktop_id", "name"],
    );
    assert_eq!(launch["properties"]["status"]["const"], "requested");
    assert_eq!(launch["properties"]["desktop_id"]["type"], "string");
    assert_eq!(launch["properties"]["name"]["type"], "string");

    let observation_schema = output_schema("observe");
    let observation = &observation_schema["oneOf"][0];
    assert_object(
        observation,
        &[
            "state_id",
            "target",
            "view",
            "element_query",
            "screenshot",
            "coordinate_spaces",
            "elements",
        ],
        &[
            "state_id",
            "target",
            "view",
            "element_query",
            "screenshot",
            "coordinate_spaces",
            "elements",
        ],
    );
    assert_eq!(observation["properties"]["state_id"], state_id());
    assert_eq!(observation["properties"]["target"]["type"], "object");
    assert_eq!(
        observation["properties"]["view"]["enum"],
        json!(["full", "visible", "interactive"])
    );
    assert_eq!(
        observation["properties"]["element_query"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        observation["properties"]["screenshot"]["properties"]["coordinate_space"]["const"],
        "screenshot_png_pixels"
    );
    assert_eq!(
        observation["properties"]["coordinate_spaces"]["type"],
        "object"
    );
    assert_eq!(observation["properties"]["elements"]["type"], "array");

    for name in TOOL_NAMES {
        let schema = output_schema(name);
        let error = &schema["oneOf"][1];
        assert_object(
            error,
            &["code", "message", "outcome", "retryable", "recovery"],
            &["code", "message", "outcome", "retryable", "recovery"],
        );
    }
}

#[test]
fn image_output_serializes_as_mcp_text_then_png() {
    let result = ToolOutput::text("state")
        .with_png_base64("cG5n")
        .into_mcp_result();
    assert_eq!(
        serde_json::to_value(result).expect("serialize result"),
        json!({
            "content": [
                {"type": "text", "text": "state"},
                {"type": "image", "data": "cG5n", "mimeType": "image/png"},
            ],
            "isError": false,
        })
    );
}

fn state_id() -> Value {
    json!({"type": "string", "pattern": "^s-[0-9a-f]{16}$"})
}

fn assert_object(schema: &Value, properties: &[&str], required: &[&str]) {
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(required));
    let actual = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, properties.iter().copied().collect());
}

fn assert_union(schema: &Value, variants: &[(&str, &[&str], &[&str])]) {
    let actual = schema["oneOf"].as_array().unwrap();
    assert_eq!(actual.len(), variants.len());
    for (action, (kind, properties, required)) in actual.iter().zip(variants) {
        assert_object(action, properties, required);
        assert_eq!(action["properties"]["type"]["const"], *kind);
    }
}
