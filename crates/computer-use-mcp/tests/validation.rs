use computer_use_mcp::validation::{
    AccessibilityScope, ActOperation, DesktopScope, ElementAction, KeyboardEvent, KeyboardFocus,
    KeyboardPoint, MAX_CLICK_COUNT, MAX_DRAG_POINTS, MAX_KEYBOARD_TRANSACTION_TEXT,
    MAX_QUERY_LENGTH, MAX_SCROLL_STEPS, MAX_TEXT_LIMIT, MAX_TREE_DEPTH, MAX_TREE_NODES,
    MAX_WAIT_STABLE_MS, MAX_WAIT_TIMEOUT_MS, ObserveCrop, ObserveView, PointerAction, TextLimit,
    ToolCall, WaitCondition, WindowAction, validate_call,
};
use serde_json::{Map, Value, json};

#[test]
fn accepts_exactly_the_six_public_tools() {
    assert_eq!(
        valid("list_desktop", json!({"scope": "windows"})),
        ToolCall::ListDesktop {
            scope: DesktopScope::Windows,
            limit: 50,
            cursor: None,
        }
    );
    assert_eq!(
        valid(
            "launch_application",
            json!({"desktop_id": "org.example.Editor.desktop"})
        ),
        ToolCall::LaunchApplication {
            desktop_id: "org.example.Editor.desktop".into()
        }
    );
    assert!(matches!(
        valid("activate_window", json!({"target": target()})),
        ToolCall::ActivateWindow { .. }
    ));
    assert!(matches!(
        valid(
            "observe",
            json!({"target": target(), "view": "both", "accessibility": {"scope": "interactive"}})
        ),
        ToolCall::Observe {
            view: ObserveView::Both,
            accessibility: Some(request),
            ..
        } if request.scope == AccessibilityScope::Interactive
    ));
    assert!(matches!(
        valid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {
                    "type": "semantic",
                    "element_id": "e-0000000000000005",
                    "action": {"type": "invoke"}
                }
            })
        ),
        ToolCall::Act {
            operation: ActOperation::Semantic {
                element_id,
                action: ElementAction::Invoke
            },
            ..
        } if element_id == "e-0000000000000005"
    ));
    assert_eq!(
        valid(
            "wait_for",
            json!({
                "target": target(),
                "condition": {"type": "frame_changed", "after_frame_id": "frame-0000000000000004"},
                "timeout_ms": 5000
            })
        ),
        ToolCall::WaitFor {
            target: Some(computer_use_mcp::validation::TargetRef {
                app_instance_id: "app-0000000000000001".into(),
                window_instance_id: "win-0000000000000002".into()
            }),
            condition: WaitCondition::FrameChanged {
                after_frame_id: "frame-0000000000000004".into()
            },
            timeout_ms: 5000
        }
    );
}

#[test]
fn list_desktop_pagination_defaults_and_bounds_are_explicit() {
    assert_eq!(
        valid(
            "list_desktop",
            json!({
                "scope": "applications",
                "limit": 7,
                "cursor": "cur-applications-0000000000001234-0000000000000007"
            })
        ),
        ToolCall::ListDesktop {
            scope: DesktopScope::Applications,
            limit: 7,
            cursor: Some("cur-applications-0000000000001234-0000000000000007".into()),
        }
    );
    assert!(
        invalid("list_desktop", json!({"scope": "windows", "limit": 0})).contains("from 1 through")
    );
    assert!(
        invalid("list_desktop", json!({"scope": "windows", "limit": 101}))
            .contains("from 1 through")
    );
    assert!(
        invalid("list_desktop", json!({"scope": "windows", "cursor": ""})).contains("non-empty")
    );
    assert!(
        invalid("list_desktop", json!({"scope": "windows", "cursor": 1}))
            .contains("must be a string")
    );
}

#[test]
fn accessibility_defaults_and_limits_are_bounded() {
    let call = valid(
        "observe",
        json!({
            "target": target(),
            "view": "accessibility",
            "accessibility": {
                "scope": "visible",
                "query": " button ",
                "limits": {"text_limit": "max", "max_nodes": 5000, "max_depth": 128}
            }
        }),
    );
    let ToolCall::Observe {
        accessibility: Some(request),
        ..
    } = call
    else {
        panic!("expected target observation");
    };
    assert_eq!(request.scope, AccessibilityScope::Visible);
    assert_eq!(request.query.as_deref(), Some("button"));
    assert_eq!(request.limits.text, TextLimit::Max);
    assert_eq!(request.limits.nodes, 5000);
    assert_eq!(request.limits.depth, 128);

    assert!(invalid(
        "observe",
        json!({"target": target(), "view": "accessibility", "accessibility": {"limits": {"max_nodes": 0}}})
    )
    .contains("from 1 through"));
    assert!(
        invalid(
            "observe",
            json!({"target": target(), "view": "accessibility", "accessibility": {"query": "   "}})
        )
        .contains("must not be blank")
    );
}

#[test]
fn observe_crop_defaults_to_monitor_and_rejects_unknown_values() {
    let call = valid("observe", json!({"target": target(), "view": "screenshot"}));
    assert!(matches!(
        call,
        ToolCall::Observe {
            crop: ObserveCrop::Monitor,
            ..
        }
    ));
    let call = valid(
        "observe",
        json!({"target": target(), "view": "screenshot", "crop": "target_window"}),
    );
    assert!(matches!(
        call,
        ToolCall::Observe {
            crop: ObserveCrop::TargetWindow,
            ..
        }
    ));
    assert!(
        invalid(
            "observe",
            json!({"target": target(), "view": "screenshot", "crop": "window"})
        )
        .contains("crop")
    );
    assert!(
        invalid(
            "observe",
            json!({"target": target(), "view": "screenshot", "crop": 1})
        )
        .contains("crop")
    );
}

#[test]
fn bounded_strings_count_raw_unicode_scalars_before_normalizing() {
    enum Field {
        Cursor,
        Query,
        Key,
        State,
    }

    for (field, maximum) in [
        (Field::Cursor, 128),
        (Field::Query, MAX_QUERY_LENGTH),
        (Field::Key, MAX_QUERY_LENGTH),
        (Field::State, MAX_QUERY_LENGTH),
    ] {
        let accepted = format!(" {} ", "界".repeat(maximum - 2));
        let overflow = format!(" {} ", "界".repeat(maximum - 1));
        let (name, accepted_call, rejected_call) = match field {
            Field::Cursor => (
                "cursor",
                json!({"scope": "windows", "cursor": accepted}),
                json!({"scope": "windows", "cursor": overflow}),
            ),
            Field::Query => (
                "query",
                json!({"target": target(), "view": "accessibility", "accessibility": {"query": accepted}}),
                json!({"target": target(), "view": "accessibility", "accessibility": {"query": overflow}}),
            ),
            Field::Key => (
                "key",
                keyboard_call(json!([{"type": "press", "key": accepted}])),
                keyboard_call(json!([{"type": "press", "key": overflow}])),
            ),
            Field::State => (
                "state",
                wait_call(
                    json!({"type": "element_state", "observation_id": "obs-0000000000000003", "element_id": "e-0000000000000005", "state": accepted}),
                    1,
                ),
                wait_call(
                    json!({"type": "element_state", "observation_id": "obs-0000000000000003", "element_id": "e-0000000000000005", "state": overflow}),
                    1,
                ),
            ),
        };
        let tool = match name {
            "cursor" => "list_desktop",
            "query" => "observe",
            "key" => "act",
            "state" => "wait_for",
            _ => unreachable!(),
        };
        let call = valid(tool, accepted_call);
        invalid(tool, rejected_call);

        match (name, call) {
            (
                "cursor",
                ToolCall::ListDesktop {
                    cursor: Some(cursor),
                    ..
                },
            ) => {
                assert_eq!(cursor.chars().count(), maximum);
            }
            (
                "query",
                ToolCall::Observe {
                    accessibility: Some(request),
                    ..
                },
            ) => {
                assert_eq!(request.query.expect("query").chars().count(), maximum - 2);
            }
            (
                "key",
                ToolCall::Act {
                    operation: ActOperation::Keyboard { events, .. },
                    ..
                },
            ) => {
                assert_eq!(events[0], KeyboardEvent::Press("界".repeat(maximum - 2)));
            }
            (
                "state",
                ToolCall::WaitFor {
                    condition: WaitCondition::ElementState { state, .. },
                    ..
                },
            ) => {
                assert_eq!(state, "界".repeat(maximum - 2));
            }
            _ => {}
        }
    }

    for (tool, call) in [
        (
            "observe",
            json!({"target": target(), "view": "accessibility", "accessibility": {"query": " \t "}}),
        ),
        (
            "act",
            keyboard_call(json!([{"type": "press", "key": " \t "}])),
        ),
        (
            "wait_for",
            wait_call(
                json!({"type": "element_state", "observation_id": "obs-0000000000000003", "element_id": "e-0000000000000005", "state": " \t "}),
                1,
            ),
        ),
    ] {
        assert!(invalid(tool, call).contains("blank"), "{tool}");
    }
}

#[test]
fn schema_bounded_inputs_accept_maxima_and_reject_overflow() {
    #[derive(Clone, Copy)]
    enum Boundary {
        Click,
        Scroll,
        Drag,
        Query,
        KeyboardText,
        TextLimit,
        Nodes,
        Depth,
        WaitTimeout,
        WaitStable,
        Named,
        SetValue,
    }

    for (case, maximum) in [
        (Boundary::Click, MAX_CLICK_COUNT),
        (Boundary::Scroll, MAX_SCROLL_STEPS as usize),
        (Boundary::Drag, MAX_DRAG_POINTS),
        (Boundary::Query, MAX_QUERY_LENGTH),
        (Boundary::KeyboardText, MAX_KEYBOARD_TRANSACTION_TEXT),
        (Boundary::TextLimit, MAX_TEXT_LIMIT),
        (Boundary::Nodes, MAX_TREE_NODES),
        (Boundary::Depth, MAX_TREE_DEPTH),
        (Boundary::WaitTimeout, MAX_WAIT_TIMEOUT_MS as usize),
        (Boundary::WaitStable, MAX_WAIT_STABLE_MS as usize),
        (Boundary::Named, MAX_QUERY_LENGTH),
        (Boundary::SetValue, MAX_TEXT_LIMIT),
    ] {
        let build = |value: usize| match case {
            Boundary::Click => (
                "act",
                pointer_call(json!({"type": "click", "x": 1, "y": 2, "count": value})),
            ),
            Boundary::Scroll => (
                "act",
                pointer_call(
                    json!({"type": "scroll", "x": 1, "y": 2, "direction": "down", "steps": value}),
                ),
            ),
            Boundary::Drag => (
                "act",
                pointer_call(
                    json!({"type": "drag", "path": (0..value).map(|point| json!({"x": point, "y": point})).collect::<Vec<_>>()}),
                ),
            ),
            Boundary::Query => (
                "observe",
                json!({"target": target(), "view": "accessibility", "accessibility": {"query": "q".repeat(value)}}),
            ),
            Boundary::KeyboardText => (
                "act",
                keyboard_call(json!([{"type": "type", "text": "t".repeat(value)}])),
            ),
            Boundary::TextLimit => (
                "observe",
                json!({"target": target(), "view": "accessibility", "accessibility": {"limits": {"text_limit": value}}}),
            ),
            Boundary::Nodes => (
                "observe",
                json!({"target": target(), "view": "accessibility", "accessibility": {"limits": {"max_nodes": value}}}),
            ),
            Boundary::Depth => (
                "observe",
                json!({"target": target(), "view": "accessibility", "accessibility": {"limits": {"max_depth": value}}}),
            ),
            Boundary::WaitTimeout => (
                "wait_for",
                wait_call(json!({"type": "frame_stable", "for_ms": 1}), value as u64),
            ),
            Boundary::WaitStable => (
                "wait_for",
                wait_call(json!({"type": "frame_stable", "for_ms": value}), 1),
            ),
            Boundary::Named => (
                "act",
                semantic_call(json!({"type": "named", "name": "n".repeat(value)})),
            ),
            Boundary::SetValue => (
                "act",
                semantic_call(json!({"type": "set_value", "value": "v".repeat(value)})),
            ),
        };
        let (tool, accepted) = build(maximum);
        valid(tool, accepted);
        let (tool, overflow) = build(maximum + 1);
        invalid(tool, overflow);
    }
}

#[test]
fn representative_nested_unknown_fields_fail_closed() {
    for (tool, call) in [
        (
            "activate_window",
            json!({"target": {"app_instance_id": "app-0000000000000001", "window_instance_id": "win-0000000000000002", "extra": true}}),
        ),
        (
            "observe",
            json!({"target": target(), "view": "accessibility", "accessibility": {"limits": {"extra": true}}}),
        ),
        (
            "act",
            pointer_call(json!({"type": "click", "x": 1, "y": 2, "extra": true})),
        ),
        (
            "act",
            keyboard_call(json!([{"type": "press", "key": "F1", "extra": true}])),
        ),
        (
            "wait_for",
            wait_call(
                json!({"type": "frame_stable", "for_ms": 1, "extra": true}),
                1,
            ),
        ),
    ] {
        assert!(invalid(tool, call).contains("unknown argument"), "{tool}");
    }
}

#[test]
fn pointer_drag_and_keyboard_transactions_preserve_all_events() {
    let pointer = valid(
        "act",
        json!({
            "target": target(),
            "source_observation": {"observation_id": "obs-0000000000000003", "frame_id": "frame-0000000000000004"},
            "operation": {
                "type": "pointer",
                "action": {"type": "drag", "path": [{"x": 1, "y": 2}, {"x": 3, "y": 4}, {"x": 5, "y": 6}]}
            }
        }),
    );
    assert!(matches!(
        pointer,
        ToolCall::Act {
            operation: ActOperation::Pointer {
                action: PointerAction::Drag { path }
            },
            ..
        } if path.len() == 3
    ));

    let keyboard = valid(
        "act",
        json!({
            "target": target(),
            "source_observation": observation(),
            "operation": {
                "type": "keyboard",
                "focus": {"type": "point", "x": 1, "y": 2},
                "events": [{"type": "press", "key": "Ctrl+L"}, {"type": "press", "key": "Enter"}]
            }
        }),
    );
    assert!(matches!(
        keyboard,
        ToolCall::Act {
            operation: ActOperation::Keyboard {
                focus,
                events
            },
            ..
        } if focus == (KeyboardFocus::Point(KeyboardPoint { x: 1.0, y: 2.0 }))
            && events == [KeyboardEvent::Press("Ctrl+L".into()), KeyboardEvent::Press("Enter".into())]
    ));
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {
                    "type": "keyboard",
                    "focus": {"type": "element", "element_id": "e-0000000000000005"},
                    "events": [{"type": "press", "key": "Enter"}]
                }
            })
        )
        .contains("must be a point")
    );
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {"type": "keyboard", "focus": {"type": "point", "x": 1, "y": 2}, "events": [{"type": "press", "key": "Alt+Tab"}]}
            })
        )
        .contains("Alt+Tab")
    );

    // Semantic focus needs no screenshot point: keyboard and paste may
    // target a focused element without a source frame.
    let semantic_focus = valid(
        "act",
        json!({
            "target": target(),
            "source_observation": {"observation_id": "obs-0000000000000003"},
            "operation": {
                "type": "keyboard",
                "focus": {"type": "semantic", "element_id": "e-0000000000000005"},
                "events": [{"type": "press", "key": "Enter"}]
            }
        }),
    );
    assert!(matches!(
        semantic_focus,
        ToolCall::Act {
            operation: ActOperation::Keyboard {
                focus: KeyboardFocus::Semantic { element_id },
                ..
            },
            ..
        } if element_id == "e-0000000000000005"
    ));
    let semantic_paste = valid(
        "act",
        json!({
            "target": target(),
            "source_observation": {"observation_id": "obs-0000000000000003"},
            "operation": {
                "type": "paste",
                "focus": {"type": "semantic", "element_id": "e-0000000000000005"},
                "text": "hello"
            }
        }),
    );
    assert!(matches!(
        semantic_paste,
        ToolCall::Act {
            operation: ActOperation::Paste {
                focus: KeyboardFocus::Semantic { element_id },
                ..
            },
            ..
        } if element_id == "e-0000000000000005"
    ));
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": {"observation_id": "obs-0000000000000003"},
                "operation": {
                    "type": "keyboard",
                    "focus": {"type": "semantic", "element_id": "not-an-id"},
                    "events": [{"type": "press", "key": "Enter"}]
                }
            })
        )
        .contains("element_id")
    );
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": {"observation_id": "obs-0000000000000003"},
                "operation": {
                    "type": "keyboard",
                    "focus": {"type": "point", "x": 1, "y": 2},
                    "events": [{"type": "press", "key": "Enter"}]
                }
            })
        )
        .contains("frame_id is required")
    );

    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": {"observation_id": "obs-0000000000000003"},
                "operation": {
                    "type": "pointer",
                    "action": {"type": "move", "x": 1, "y": 2}
                }
            })
        )
        .contains("frame_id is required")
    );

    let semantic_without_frame = valid(
        "act",
        json!({
            "target": target(),
            "source_observation": {"observation_id": "obs-0000000000000003"},
            "operation": {
                "type": "semantic",
                "element_id": "e-0000000000000005",
                "action": {"type": "invoke"}
            }
        }),
    );
    assert!(matches!(semantic_without_frame, ToolCall::Act { .. }));
}

#[test]
fn keyboard_event_grammar_is_strict_and_bounded() {
    let focus = json!({"type": "point", "x": 1, "y": 2});
    let target = target();
    let source = observation();
    let press = |key| json!({"type": "press", "key": key});
    let type_event = |text: &str| json!({"type": "type", "text": text});
    let call = |events| {
        json!({
            "target": target.clone(),
            "source_observation": source.clone(),
            "operation": {"type": "keyboard", "focus": focus.clone(), "events": events}
        })
    };

    let bounded_presses = (0..8).map(|_| press("F1")).collect::<Vec<_>>();
    assert!(matches!(
        valid("act", call(bounded_presses)),
        ToolCall::Act {
            operation: ActOperation::Keyboard { events, .. },
            ..
        } if events.len() == 8
    ));
    assert!(matches!(
        valid("act", call(vec![type_event("hello")])),
        ToolCall::Act {
            operation: ActOperation::Keyboard { events, .. },
            ..
        } if events == [KeyboardEvent::Type("hello".into())]
    ));

    for events in [
        vec![press("Ctrl+L"), type_event("hello")],
        vec![press("Tab"), type_event("hello")],
        vec![press("F6"), type_event("hello")],
        vec![type_event("hello"), press("Enter")],
    ] {
        assert!(invalid("act", call(events)).contains("mixed"));
    }
    assert!(invalid("act", call(vec![type_event("")])).contains("must not be empty"));
    assert!(invalid("act", call(vec![type_event("\0")])).contains("NUL"));
    assert!(invalid("act", call(vec![type_event(&"x".repeat(4_097))])).contains("4096"));
    assert!(
        invalid("act", call(vec![press("Ctrl+Alt+Shift+Super+Meta+F1")]))
            .contains("at most 4 modifiers")
    );
    assert!(
        invalid("act", call((0..9).map(|_| press("F1")).collect::<Vec<_>>()))
            .contains("1 through 8")
    );
}

#[test]
fn representative_union_variants_match_runtime_validation() {
    for operation in [
        json!({"type": "pointer", "action": {"type": "move", "x": 1, "y": 2}}),
        json!({"type": "pointer", "action": {"type": "click", "x": 1, "y": 2, "button": "right", "count": 2}}),
        json!({"type": "pointer", "action": {"type": "scroll", "x": 1, "y": 2, "direction": "down", "steps": 3}}),
        json!({"type": "semantic", "element_id": "e-0000000000000005", "action": {"type": "focus"}}),
        json!({"type": "semantic", "element_id": "e-0000000000000005", "action": {"type": "named", "name": "open"}}),
        json!({"type": "semantic", "element_id": "e-0000000000000005", "action": {"type": "set_value", "value": "text"}}),
    ] {
        valid(
            "act",
            json!({"target": target(), "source_observation": observation(), "operation": operation}),
        );
    }

    for condition in [
        json!({"type": "frame_advanced", "after_frame_id": "frame-0000000000000004"}),
        json!({"type": "frame_stable", "for_ms": 0}),
        json!({"type": "accessibility_advanced", "after_observation_id": "obs-0000000000000003"}),
        json!({"type": "element_state", "observation_id": "obs-0000000000000003", "element_id": "e-0000000000000005", "state": "focused"}),
        json!({"type": "element_value", "observation_id": "obs-0000000000000003", "element_id": "e-0000000000000005", "value": "text"}),
    ] {
        valid(
            "wait_for",
            json!({"target": target(), "condition": condition, "timeout_ms": 1}),
        );
    }
}

#[test]
fn opaque_ids_bounds_and_unknown_fields_fail_closed() {
    assert!(invalid("not_a_tool", json!({})).contains("unknown tool"));
    assert!(invalid(
        "activate_window",
        json!({"target": {"app_instance_id": "Editor", "window_instance_id": "win-0000000000000002"}})
    )
    .contains("opaque"));
    assert!(
        invalid(
            "observe",
            json!({"target": target(), "view": "both", "unexpected": true})
        )
        .contains("unknown argument")
    );
    assert!(invalid(
        "act",
        json!({
            "target": target(),
            "source_observation": observation(),
            "operation": {"type": "semantic", "element_id": "element-5", "action": {"type": "invoke"}}
        })
    )
    .contains("opaque"));
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {"type": "pointer", "action": {"type": "click", "x": -1, "y": 2}}
            })
        )
        .contains("non-negative")
    );
    assert!(
        invalid(
            "wait_for",
            json!({
                "target": target(),
                "condition": {"type": "frame_stable", "for_ms": 1, "unexpected": true},
                "timeout_ms": 1
            })
        )
        .contains("unknown argument")
    );
    assert!(
        invalid(
            "launch_application",
            json!({"desktop_id": "org.example.Editor"})
        )
        .contains("ending in .desktop")
    );
}

fn pointer_call(action: Value) -> Value {
    json!({
        "target": target(),
        "source_observation": observation(),
        "operation": {"type": "pointer", "action": action}
    })
}

fn keyboard_call(events: Value) -> Value {
    json!({
        "target": target(),
        "source_observation": observation(),
        "operation": {
            "type": "keyboard",
            "focus": {"type": "point", "x": 1, "y": 2},
            "events": events
        }
    })
}

fn semantic_call(action: Value) -> Value {
    json!({
        "target": target(),
        "source_observation": observation(),
        "operation": {
            "type": "semantic",
            "element_id": "e-0000000000000005",
            "action": action
        }
    })
}

fn wait_call(condition: Value, timeout_ms: u64) -> Value {
    json!({"target": target(), "condition": condition, "timeout_ms": timeout_ms})
}

fn target() -> Value {
    json!({
        "app_instance_id": "app-0000000000000001",
        "window_instance_id": "win-0000000000000002"
    })
}

fn observation() -> Value {
    json!({
        "observation_id": "obs-0000000000000003",
        "frame_id": "frame-0000000000000004"
    })
}

fn valid(name: &str, arguments: Value) -> ToolCall {
    validate_call(name, object(arguments)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn invalid(name: &str, arguments: Value) -> String {
    validate_call(name, object(arguments))
        .expect_err("call should be rejected")
        .to_string()
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object arguments")
}

#[test]
fn activate_window_action_defaults_to_activate_and_parses_all_variants() {
    assert!(matches!(
        valid("activate_window", json!({"target": target()})),
        ToolCall::ActivateWindow {
            action: WindowAction::Activate,
            ..
        }
    ));
    for (name, expected) in [
        ("activate", WindowAction::Activate),
        ("minimize", WindowAction::Minimize),
        ("maximize", WindowAction::Maximize),
        ("restore", WindowAction::Restore),
        ("close", WindowAction::Close),
    ] {
        assert_eq!(
            valid(
                "activate_window",
                json!({"target": target(), "action": name})
            ),
            ToolCall::ActivateWindow {
                target: computer_use_mcp::validation::TargetRef {
                    app_instance_id: "app-0000000000000001".into(),
                    window_instance_id: "win-0000000000000002".into()
                },
                action: expected,
            }
        );
    }
    assert!(
        invalid(
            "activate_window",
            json!({"target": target(), "action": "hide"})
        )
        .contains("must be activate, minimize, maximize, restore, or close")
    );
    assert!(
        invalid("activate_window", json!({"target": target(), "action": 1}))
            .contains("must be a string")
    );
    assert!(
        invalid(
            "activate_window",
            json!({"target": target(), "action": "activate", "unexpected": true})
        )
        .contains("unknown argument")
    );
}

#[test]
fn paste_operation_is_bounded_and_requires_explicit_focus() {
    assert!(matches!(
        valid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {
                    "type": "paste",
                    "focus": {"type": "point", "x": 1, "y": 2},
                    "text": "hello"
                }
            })
        ),
        ToolCall::Act {
            operation: ActOperation::Paste { text, .. },
            ..
        } if text == "hello"
    ));
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {
                    "type": "paste",
                    "focus": {"type": "point", "x": 1, "y": 2},
                    "text": ""
                }
            })
        )
        .contains("must not be empty")
    );
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation(),
                "operation": {
                    "type": "paste",
                    "focus": {"type": "point", "x": 1, "y": 2},
                    "text": "x".repeat(MAX_TEXT_LIMIT + 1)
                }
            })
        )
        .contains("at most")
    );
    assert!(
        invalid(
            "act",
            json!({
                "target": target(),
                "source_observation": observation_with_frame(None),
                "operation": {
                    "type": "paste",
                    "focus": {"type": "point", "x": 1, "y": 2},
                    "text": "hello"
                }
            })
        )
        .contains("frame_id")
    );
}

#[test]
fn window_open_and_close_conditions_parse_and_reject_unknown() {
    assert_eq!(
        valid(
            "wait_for",
            json!({
                "condition": {"type": "window_opened", "desktop_id": "org.example.Editor.desktop"},
                "timeout_ms": 5000
            })
        ),
        ToolCall::WaitFor {
            target: None,
            condition: WaitCondition::WindowOpened {
                desktop_id: "org.example.Editor.desktop".into()
            },
            timeout_ms: 5000
        }
    );
    assert_eq!(
        valid(
            "wait_for",
            json!({
                "condition": {"type": "window_closed", "window_instance_id": "win-0000000000000009"},
                "timeout_ms": 5000
            })
        ),
        ToolCall::WaitFor {
            target: None,
            condition: WaitCondition::WindowClosed {
                window_instance_id: "win-0000000000000009".into()
            },
            timeout_ms: 5000
        }
    );
    assert!(
        invalid(
            "wait_for",
            json!({
                "condition": {"type": "window_opened", "desktop_id": "org.example Editor"},
                "timeout_ms": 1
            })
        )
        .contains("non-whitespace application ID")
    );
    assert!(matches!(
        valid(
            "wait_for",
            json!({
                "condition": {"type": "window_opened", "desktop_id": "org.example.Editor"},
                "timeout_ms": 1
            })
        ),
        ToolCall::WaitFor {
            target: None,
            condition: WaitCondition::WindowOpened { desktop_id },
            timeout_ms: 1
        } if desktop_id == "org.example.Editor"
    ));
    assert!(
        invalid(
            "wait_for",
            json!({
                "target": target(),
                "condition": {"type": "window_closed", "window_instance_id": "window-9"},
                "timeout_ms": 1
            })
        )
        .contains("opaque")
    );
    assert!(invalid(
        "wait_for",
        json!({
            "target": target(),
            "condition": {"type": "window_closed", "window_instance_id": "win-0000000000000009"},
            "timeout_ms": 1
        })
    )
    .contains("must match"));
    assert!(
        invalid(
            "wait_for",
            json!({
                "target": target(),
                "condition": {"type": "window_opened", "desktop_id": "org.example.Editor.desktop", "unexpected": true},
                "timeout_ms": 1
            })
        )
        .contains("unknown argument")
    );
}

fn observation_with_frame(frame_id: Option<&str>) -> Value {
    match frame_id {
        Some(frame_id) => json!({
            "observation_id": "obs-0000000000000003",
            "frame_id": frame_id
        }),
        None => json!({"observation_id": "obs-0000000000000003"}),
    }
}
