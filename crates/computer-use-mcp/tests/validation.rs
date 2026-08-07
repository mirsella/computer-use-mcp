use computer_use_mcp::validation::{
    ApplicationScope, ElementAction, KeyboardAction, KeyboardFocus, MAX_CLICK_COUNT,
    MAX_QUERY_LENGTH, MAX_SCROLL_STEPS, MAX_TEXT_LIMIT, MAX_TREE_DEPTH, MAX_TREE_NODES,
    MouseButton, ObservationView, PointerAction, TextLimit, ToolCall, validate_call,
};
use serde_json::{Map, Value, json};

const STATE_ID: &str = "s-0000000000000001";

#[test]
fn accepts_all_six_tools() {
    assert_eq!(
        valid("list_applications", json!({"scope": "running"})),
        ToolCall::ListApplications {
            scope: ApplicationScope::Running,
        }
    );
    assert_eq!(
        valid(
            "launch_application",
            json!({"desktop_id": "org.example.Editor.desktop"})
        ),
        ToolCall::LaunchApplication {
            desktop_id: "org.example.Editor.desktop".into(),
        }
    );
    assert_eq!(
        valid(
            "observe",
            json!({"target": " Editor ", "text_limit": "max", "max_tree_nodes": 12, "max_tree_depth": 3})
        ),
        ToolCall::Observe {
            target: "Editor".into(),
            view: ObservationView::Full,
            query: None,
            text_limit: Some(TextLimit::Max),
            max_tree_nodes: Some(12),
            max_tree_depth: Some(3),
        }
    );
    assert_eq!(
        valid(
            "act_on_element",
            json!({"state_id": STATE_ID, "element_id": 7, "action": {"type": "focus"}})
        ),
        ToolCall::ActOnElement {
            state_id: STATE_ID.into(),
            element_id: "7".into(),
            action: ElementAction::Focus,
        }
    );
    assert_eq!(
        valid(
            "pointer",
            json!({"state_id": STATE_ID, "action": {"type": "move", "x": 1.5, "y": 2}})
        ),
        ToolCall::Pointer {
            state_id: STATE_ID.into(),
            action: PointerAction::Move { x: 1.5, y: 2.0 },
        }
    );
    assert_eq!(
        valid(
            "keyboard",
            json!({"state_id": STATE_ID, "focus": {"x": 3, "y": 4}, "action": {"type": "press", "key": "Enter"}})
        ),
        ToolCall::Keyboard {
            state_id: STATE_ID.into(),
            focus: KeyboardFocus::Point((3.0, 4.0)),
            action: KeyboardAction::Press("Enter".into()),
        }
    );
}

#[test]
fn parses_nested_discriminated_actions_and_defaults() {
    assert_eq!(
        element_action(json!({"type": "invoke"})),
        ElementAction::Invoke
    );
    assert_eq!(
        element_action(json!({"type": "named", "name": " activate "})),
        ElementAction::Named(" activate ".into())
    );
    assert_eq!(
        element_action(json!({"type": "set_value", "value": "value"})),
        ElementAction::SetValue("value".into())
    );
    assert_eq!(
        pointer_action(json!({"type": "click", "x": 1, "y": 2})),
        PointerAction::Click {
            x: 1.0,
            y: 2.0,
            button: MouseButton::Left,
            count: 1,
        }
    );
    assert_eq!(
        pointer_action(json!({"type": "drag", "from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4})),
        PointerAction::Drag {
            from: (1.0, 2.0),
            to: (3.0, 4.0),
        }
    );
    assert_eq!(
        keyboard_action(json!({"type": "type", "text": "hello"})),
        KeyboardAction::Type("hello".into())
    );
    assert!(matches!(
        valid("observe", json!({"target": "Editor"})),
        ToolCall::Observe {
            view: ObservationView::Full,
            query: None,
            text_limit: None,
            max_tree_nodes: None,
            max_tree_depth: None,
            ..
        }
    ));
}

#[test]
fn parses_observation_views_queries_and_semantic_keyboard_focus() {
    for (view, expected) in [
        ("full", ObservationView::Full),
        ("visible", ObservationView::Visible),
        ("interactive", ObservationView::Interactive),
    ] {
        assert!(matches!(
            valid("observe", json!({"target": "Editor", "view": view})),
            ToolCall::Observe {
                view: actual,
                query: None,
                ..
            } if actual == expected
        ));
    }
    assert!(matches!(
        valid(
            "observe",
            json!({"target": "Editor", "query": " button "})
        ),
        ToolCall::Observe {
            query: Some(query),
            ..
        } if query == "button"
    ));
    assert_eq!(
        valid(
            "keyboard",
            json!({"state_id": STATE_ID, "focus": {"element_id": "007"}, "action": {"type": "type", "text": "hello"}})
        ),
        ToolCall::Keyboard {
            state_id: STATE_ID.into(),
            focus: KeyboardFocus::Element("007".into()),
            action: KeyboardAction::Type("hello".into()),
        }
    );
}

#[test]
fn validates_numbers_and_bounded_counts() {
    assert_eq!(
        valid(
            "observe",
            json!({"target": "Editor", "text_limit": 12.0, "max_tree_nodes": 12.0, "max_tree_depth": 3.0})
        ),
        ToolCall::Observe {
            target: "Editor".into(),
            view: ObservationView::Full,
            query: None,
            text_limit: Some(TextLimit::Count(12)),
            max_tree_nodes: Some(12),
            max_tree_depth: Some(3),
        }
    );
    assert_eq!(
        pointer_action(json!({"type": "click", "x": 1, "y": 2, "count": 2.0})),
        PointerAction::Click {
            x: 1.0,
            y: 2.0,
            button: MouseButton::Left,
            count: 2,
        }
    );
    assert_eq!(
        pointer_action(
            json!({"type": "scroll", "x": 1, "y": 2, "direction": "down", "steps": 2.0})
        ),
        PointerAction::Scroll {
            x: 1.0,
            y: 2.0,
            delta_x: 0,
            delta_y: 240,
        }
    );
    for arguments in [
        json!({"state_id": STATE_ID, "action": {"type": "move", "x": "1", "y": 2}}),
        json!({"state_id": STATE_ID, "action": {"type": "drag", "from_x": 1, "from_y": null, "to_x": 3, "to_y": 4}}),
        json!({"state_id": STATE_ID, "focus": {"x": true, "y": 2}, "action": {"type": "press", "key": "x"}}),
    ] {
        assert!(
            invalid(
                if arguments.get("focus").is_some() {
                    "keyboard"
                } else {
                    "pointer"
                },
                arguments
            )
            .contains("number")
        );
    }
    for count in [json!(0), json!(-1), json!(1.5), json!("1")] {
        assert!(invalid("pointer", json!({"state_id": STATE_ID, "action": {"type": "click", "x": 1, "y": 2, "count": count}})).contains("integer from 1 through"));
        assert!(invalid("pointer", json!({"state_id": STATE_ID, "action": {"type": "scroll", "x": 1, "y": 2, "direction": "down", "steps": count}})).contains("integer from 1 through"));
    }
    for key in ["max_tree_nodes", "max_tree_depth"] {
        assert!(
            invalid("observe", json!({"target": "Editor", key: 0}))
                .contains("integer from 1 through")
        );
    }
    assert_eq!(
        valid("observe", json!({"target": "Editor", "text_limit": 0})),
        ToolCall::Observe {
            target: "Editor".into(),
            view: ObservationView::Full,
            query: None,
            text_limit: Some(TextLimit::Count(0)),
            max_tree_nodes: None,
            max_tree_depth: None,
        }
    );
}

#[test]
fn rejects_values_over_contract_limits() {
    assert!(
        invalid(
            "pointer",
            json!({"state_id": STATE_ID, "action": {"type": "click", "x": 1, "y": 2, "count": MAX_CLICK_COUNT + 1}}),
        )
        .contains(&format!("through {MAX_CLICK_COUNT}"))
    );
    assert!(
        invalid(
            "pointer",
            json!({"state_id": STATE_ID, "action": {"type": "scroll", "x": 1, "y": 2, "direction": "down", "steps": MAX_SCROLL_STEPS + 1}}),
        )
        .contains(&format!("through {MAX_SCROLL_STEPS}"))
    );
    assert!(
        invalid(
            "observe",
            json!({"target": "Editor", "text_limit": MAX_TEXT_LIMIT + 1}),
        )
        .contains(&format!("must not exceed {MAX_TEXT_LIMIT}"))
    );
    for (key, maximum) in [
        ("max_tree_nodes", MAX_TREE_NODES),
        ("max_tree_depth", MAX_TREE_DEPTH),
    ] {
        assert!(
            invalid("observe", json!({"target": "Editor", key: maximum + 1}),)
                .contains(&format!("through {maximum}"))
        );
    }
}

#[test]
fn rejects_negative_coordinates() {
    for action in [
        json!({"type": "move", "x": -1, "y": 0}),
        json!({"type": "click", "x": 0, "y": -1}),
        json!({"type": "drag", "from_x": -1, "from_y": 0, "to_x": 1, "to_y": 2}),
        json!({"type": "drag", "from_x": 0, "from_y": -1, "to_x": 1, "to_y": 2}),
        json!({"type": "drag", "from_x": 0, "from_y": 1, "to_x": -1, "to_y": 2}),
        json!({"type": "drag", "from_x": 0, "from_y": 1, "to_x": 2, "to_y": -1}),
        json!({"type": "scroll", "x": -1, "y": 0, "direction": "down"}),
        json!({"type": "scroll", "x": 0, "y": -1, "direction": "down"}),
    ] {
        assert!(
            invalid("pointer", json!({"state_id": STATE_ID, "action": action}),)
                .contains("must be non-negative")
        );
    }
    for focus in [json!({"x": -1, "y": 0}), json!({"x": 0, "y": -1})] {
        assert!(invalid(
            "keyboard",
            json!({"state_id": STATE_ID, "focus": focus, "action": {"type": "press", "key": "x"}}),
        )
        .contains("must be non-negative"));
    }
}

#[test]
fn converts_scroll_directions_to_wheel_deltas() {
    for (direction, delta_x, delta_y) in [
        ("up", 0, -240),
        ("down", 0, 240),
        ("left", -240, 0),
        ("right", 240, 0),
    ] {
        assert_eq!(
            pointer_action(
                json!({"type": "scroll", "x": 10, "y": 20, "direction": direction, "steps": 2})
            ),
            PointerAction::Scroll {
                x: 10.0,
                y: 20.0,
                delta_x,
                delta_y,
            }
        );
    }
    assert!(invalid("pointer", json!({"state_id": STATE_ID, "action": {"type": "scroll", "x": 1, "y": 2, "direction": "north"}})).contains("direction"));
}

#[test]
fn rejects_unknown_fields_in_nested_objects() {
    assert!(
        invalid(
            "act_on_element",
            json!({"state_id": STATE_ID, "element_id": 1, "action": "focus"}),
        )
        .contains("argument \"action\" must be an object")
    );
    assert!(
        invalid(
            "pointer",
            json!({"state_id": STATE_ID, "action": {"type": "move", "x": 1, "y": 2, "extra": true}})
        )
        .contains("unknown argument")
    );
    assert!(invalid("keyboard", json!({"state_id": STATE_ID, "focus": {"x": 1, "y": 2, "extra": true}, "action": {"type": "press", "key": "x"}})).contains("unknown argument"));
    assert!(invalid("keyboard", json!({"state_id": STATE_ID, "focus": {"element_id": 1, "extra": true}, "action": {"type": "press", "key": "x"}})).contains("unknown argument"));
    assert!(invalid(
        "act_on_element",
        json!({"state_id": STATE_ID, "element_id": 1, "action": {"type": "invoke", "extra": true}})
    )
    .contains("unknown argument"));
    assert!(
        invalid("observe", json!({"target": "Editor", "extra": true})).contains("unknown argument")
    );
}

#[test]
fn rejects_malformed_or_padded_state_ids() {
    for state_id in [
        " state ",
        " s-0000000000000001",
        "s-0000000000000001 ",
        "s-000000000000000",
        "s-00000000000000001",
        "s-000000000000000G",
        "S-0000000000000001",
    ] {
        assert!(
            invalid(
                "pointer",
                json!({"state_id": state_id, "action": {"type": "move", "x": 1, "y": 2}}),
            )
            .contains("must match s- followed by 16 lowercase hexadecimal digits")
        );
    }
}

#[test]
fn rejects_padded_or_incomplete_desktop_ids() {
    for desktop_id in [
        " org.example.Editor.desktop",
        "org.example.Editor.desktop ",
        "org.example.Editor",
        "org.example Editor.desktop",
        ".desktop",
        "",
    ] {
        assert!(
            invalid("launch_application", json!({"desktop_id": desktop_id}))
                .contains("exact non-whitespace desktop ID ending in .desktop")
        );
    }
}

#[test]
fn validates_identifiers_and_preserves_literal_text_and_values() {
    for (name, arguments) in [
        ("observe", json!({"target": ""})),
        (
            "act_on_element",
            json!({"state_id": STATE_ID, "element_id": 1, "action": {"type": "named", "name": " "}}),
        ),
        (
            "keyboard",
            json!({"state_id": STATE_ID, "focus": {"x": 1, "y": 2}, "action": {"type": "press", "key": " "}}),
        ),
    ] {
        assert!(invalid(name, arguments).contains("must not be blank"));
    }
    for (element_id, expected) in [
        (json!(0), "0"),
        (json!(1.0), "1"),
        (json!(14), "14"),
        (json!("007"), "007"),
        (json!("42"), "42"),
    ] {
        assert_eq!(
            valid(
                "act_on_element",
                json!({"state_id": STATE_ID, "element_id": element_id, "action": {"type": "focus"}})
            ),
            ToolCall::ActOnElement {
                state_id: STATE_ID.into(),
                element_id: expected.into(),
                action: ElementAction::Focus
            }
        );
    }
    for element_id in [
        json!(-1),
        json!(1.5),
        json!(5_000),
        json!("+1"),
        json!("5000"),
        json!("-1"),
        json!(" "),
        json!(true),
        Value::Null,
    ] {
        assert!(
            invalid(
                "act_on_element",
                json!({"state_id": STATE_ID, "element_id": element_id, "action": {"type": "focus"}})
            )
            .contains("element ID")
        );
    }
    assert!(invalid(
        "keyboard",
        json!({"state_id": STATE_ID, "focus": {"element_id": 5_000}, "action": {"type": "press", "key": "x"}}),
    )
    .contains("element ID"));
    for element_id in [
        Value::String("0".repeat(20)),
        Value::String("0".repeat(21)),
        json!("not-a-number"),
    ] {
        assert!(invalid(
            "act_on_element",
            json!({"state_id": STATE_ID, "element_id": element_id, "action": {"type": "focus"}}),
        )
        .contains("element ID"));
    }
    assert_eq!(
        element_action(json!({"type": "set_value", "value": ""})),
        ElementAction::SetValue("".into())
    );
    assert_eq!(
        element_action(json!({"type": "set_value", "value": "  value  "})),
        ElementAction::SetValue("  value  ".into())
    );
    assert_eq!(
        keyboard_action(json!({"type": "type", "text": ""})),
        KeyboardAction::Type("".into())
    );
    assert_eq!(
        keyboard_action(json!({"type": "type", "text": "   "})),
        KeyboardAction::Type("   ".into())
    );
}

#[test]
fn rejects_invalid_observation_views_and_queries() {
    for view in [json!("all"), json!("VISIBLE"), json!(1), Value::Null] {
        assert!(
            invalid("observe", json!({"target": "Editor", "view": view}))
                .contains("must be full, visible, or interactive")
        );
    }
    for query in [json!(""), json!("  "), json!(1), Value::Null] {
        let error = invalid("observe", json!({"target": "Editor", "query": query}));
        assert!(error.contains("must not be blank") || error.contains("must be a string"));
    }
    assert!(
        invalid(
            "observe",
            json!({"target": "Editor", "query": "x".repeat(MAX_QUERY_LENGTH + 1)})
        )
        .contains("at most 1000 characters")
    );
    assert!(
        invalid(
            "observe",
            json!({"target": "Editor", "query": format!(" {} ", "x".repeat(MAX_QUERY_LENGTH))})
        )
        .contains("at most 1000 characters")
    );
}

#[test]
fn rejects_alt_tab_shortcuts() {
    for key in ["Alt+Tab", "alt + shift + tab", "Ctrl+ALT+TAB"] {
        assert!(invalid("keyboard", json!({"state_id": STATE_ID, "focus": {"x": 1, "y": 2}, "action": {"type": "press", "key": key}})).contains("Alt+Tab"));
    }
    for key in ["Tab", "Alt+F4"] {
        assert_eq!(
            keyboard_action(json!({"type": "press", "key": key})),
            KeyboardAction::Press(key.into())
        );
    }
    assert!(
        invalid(
            "keyboard",
            json!({
                "state_id": STATE_ID,
                "focus": {"x": 1, "y": 2},
                "action": {"type": "press", "key": "Alt+Tab", "extra": true},
            })
        )
        .contains("unknown argument")
    );
}

fn element_action(action: Value) -> ElementAction {
    match valid(
        "act_on_element",
        json!({"state_id": STATE_ID, "element_id": 1, "action": action}),
    ) {
        ToolCall::ActOnElement { action, .. } => action,
        _ => unreachable!(),
    }
}

fn pointer_action(action: Value) -> PointerAction {
    match valid("pointer", json!({"state_id": STATE_ID, "action": action})) {
        ToolCall::Pointer { action, .. } => action,
        _ => unreachable!(),
    }
}

fn keyboard_action(action: Value) -> KeyboardAction {
    match valid(
        "keyboard",
        json!({"state_id": STATE_ID, "focus": {"x": 1, "y": 2}, "action": action}),
    ) {
        ToolCall::Keyboard { action, .. } => action,
        _ => unreachable!(),
    }
}

fn valid(name: &str, arguments: Value) -> ToolCall {
    validate_call(name, object(arguments)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn invalid(name: &str, arguments: Value) -> String {
    validate_call(name, object(arguments))
        .expect_err("call should fail")
        .to_string()
}

fn object(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("test arguments must be an object")
        .clone()
}
