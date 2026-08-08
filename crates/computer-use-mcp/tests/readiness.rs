use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use computer_use_mcp::{
    contract::TOOL_NAMES,
    errors::{RuntimeError, ToolOutcome},
    runtime::{DesktopRuntime, ToolOutput},
    server::ComputerUseMcpServer,
    validation::{
        ApplicationScope, ElementAction, KeyboardAction, KeyboardFocus, ObservationView,
        PointerAction, ToolCall,
    },
};
use image::{ColorType, GenericImageView, ImageEncoder, ImageFormat, codecs::png::PngEncoder};
use rmcp::{
    RoleClient, ServiceExt,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{IntoTransport, Transport},
};
use serde_json::{Value, json};
use tokio::sync::watch;

const RUNTIME_ERROR_TEXT: &str = "fake runtime unavailable\nCode: fake_runtime_unavailable\nOutcome: not_started\nRetryable: true\nRecovery: Call observe for current state, then retry only if the requested action is still needed.";
const INVALID_ARGUMENTS_TEXT: &str = "missing required argument \"type\"\nCode: invalid_arguments\nOutcome: not_started\nRetryable: true\nRecovery: Correct the arguments using the tool input schema, then retry.";

#[derive(Clone)]
struct FakeRuntime {
    state: Arc<FakeRuntimeState>,
}

struct FakeRuntimeState {
    calls: Mutex<Vec<ToolCall>>,
    fail_next: AtomicBool,
    png_base64: String,
    starts: AtomicUsize,
    ready: watch::Sender<bool>,
    shutdowns: AtomicUsize,
}

impl FakeRuntime {
    fn new(png_base64: String) -> Self {
        let (ready, _) = watch::channel(true);
        Self {
            state: Arc::new(FakeRuntimeState {
                calls: Mutex::new(Vec::new()),
                fail_next: AtomicBool::new(false),
                png_base64,
                starts: AtomicUsize::new(0),
                ready,
                shutdowns: AtomicUsize::new(0),
            }),
        }
    }

    fn gated() -> Self {
        let runtime = Self::new(String::new());
        runtime.state.ready.send_replace(false);
        runtime
    }

    fn calls(&self) -> Vec<ToolCall> {
        self.state.calls.lock().expect("fake calls lock").clone()
    }

    fn fail_next(&self) {
        self.state.fail_next.store(true, Ordering::Release);
    }

    fn shutdowns(&self) -> usize {
        self.state.shutdowns.load(Ordering::Acquire)
    }
}

impl DesktopRuntime for FakeRuntime {
    fn start(&self) {
        self.state.starts.fetch_add(1, Ordering::AcqRel);
    }

    async fn wait_for_desktop_session(&self) {
        self.state
            .ready
            .subscribe()
            .wait_for(|ready| *ready)
            .await
            .expect("fake readiness sender is retained");
    }

    fn execute(
        &self,
        call: ToolCall,
    ) -> impl std::future::Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_ {
        self.state
            .calls
            .lock()
            .expect("fake calls lock")
            .push(call.clone());
        let fail = self.state.fail_next.swap(false, Ordering::AcqRel);
        let png_base64 =
            matches!(call, ToolCall::Observe { .. }).then(|| self.state.png_base64.clone());
        async move {
            if fail {
                return Err(RuntimeError::new(
                    "fake_runtime_unavailable",
                    "fake runtime unavailable",
                    ToolOutcome::NotStarted,
                    true,
                    "Call observe for current state, then retry only if the requested action is still needed.",
                ));
            }
            let output = ToolOutput::text("fake runtime success")
                .with_structured_content(json!({"status": "fake"}));
            Ok(match png_base64 {
                Some(png_base64) => output.with_png_base64(png_base64),
                None => output,
            })
        }
    }

    async fn cleanup(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn shutdown(&self) -> impl std::future::Future<Output = Result<(), RuntimeError>> + Send + '_ {
        self.state.shutdowns.fetch_add(1, Ordering::AcqRel);
        async { Ok(()) }
    }
}

#[tokio::test]
async fn handshake_and_independent_calls_bypass_desktop_session_waiters() {
    let runtime = FakeRuntime::gated();
    let (client_transport, server) = spawn_server(&runtime);
    let mut client = IntoTransport::<RoleClient, _, _>::into_transport(client_transport);

    let initialized = tokio::time::timeout(
        Duration::from_secs(1),
        initialize(&mut client, "2025-11-25", "background-startup-test"),
    )
    .await
    .expect("handshake must not wait for desktop session");
    assert_eq!(initialized["id"], 1);

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "observe", "arguments": {"target": "Editor"}},
        }),
    )
    .await;
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "observe", "arguments": {"target": "Cancelled"}},
        }),
    )
    .await;
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 4, "reason": "test readiness cancellation"},
        }),
    )
    .await;

    for request in [
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "list_applications", "arguments": {"scope": "running"}},
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "launch_application", "arguments": {"desktop_id": "org.example.Editor.desktop"}},
        }),
    ] {
        send_json(&mut client, request).await;
    }

    let mut independent = Vec::new();
    for _ in 0..2 {
        let response = tokio::time::timeout(Duration::from_secs(1), receive_json(&mut client))
            .await
            .expect("list and launch must bypass desktop initialization");
        independent.push(response["id"].as_u64().unwrap());
    }
    independent.sort_unstable();
    assert_eq!(independent, [3, 5]);
    assert_eq!(runtime.state.starts.load(Ordering::Acquire), 1);
    assert_eq!(runtime.state.calls.lock().unwrap().len(), 2);

    runtime.state.ready.send_replace(true);
    let observed = tokio::time::timeout(Duration::from_secs(1), receive_json(&mut client))
        .await
        .expect("observe should continue after desktop initialization");
    assert_eq!(observed["id"], 2);
    assert_eq!(runtime.state.calls.lock().unwrap().len(), 3);

    stop_server(client, server, &runtime).await;
}

#[tokio::test]
async fn mcp_agent_path_dispatches_every_tool_and_preserves_error_boundaries() {
    let png = test_png();
    let runtime = FakeRuntime::new(STANDARD.encode(&png));
    let (client_transport, server) = spawn_server(&runtime);
    let mut client = IntoTransport::<RoleClient, _, _>::into_transport(client_transport);

    let initialized = initialize(&mut client, "2025-06-18", "readiness-test").await;
    assert_eq!(initialized["id"], 1);
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "computer-use-mcp"
    );
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
        }),
    )
    .await;
    let listed = receive_json(&mut client).await;
    let listed_names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(listed_names, TOOL_NAMES);

    let calls = valid_tool_calls();
    let mut observe_response = None;
    for (offset, (name, arguments)) in calls.into_iter().enumerate() {
        let id = 10 + offset as u64;
        send_json(
            &mut client,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }),
        )
        .await;
        let response = receive_json(&mut client).await;
        assert_eq!(response["id"], id, "response for {name}");
        assert_success(&response, name);
        if name == "observe" {
            observe_response = Some(response);
        }
    }

    assert_eq!(runtime.calls(), expected_tool_calls());
    assert_png_content(
        observe_response
            .as_ref()
            .expect("observe response must be present"),
        &png,
    );

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 30,
            "method": "tools/call",
            "params": {"name": "not_a_tool", "arguments": {}},
        }),
    )
    .await;
    let unknown = receive_json(&mut client).await;
    assert_eq!(unknown["id"], 30);
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(unknown.get("result").is_none());

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "tools/call",
            "params": {
                "name": "pointer",
                "arguments": {"state_id": "s-0123456789abcdef", "action": {}},
            },
        }),
    )
    .await;
    let invalid = receive_json(&mut client).await;
    assert_eq!(invalid["id"], 31);
    assert!(invalid.get("error").is_none());
    assert_eq!(invalid["result"]["isError"], true);
    assert_eq!(
        invalid["result"]["content"][0]["text"],
        INVALID_ARGUMENTS_TEXT
    );
    assert_eq!(
        invalid["result"]["structuredContent"],
        invalid_arguments_structured_content()
    );
    assert_eq!(runtime.calls(), expected_tool_calls());

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 32,
            "method": "tools/call",
            "params": {
                "name": "keyboard",
                "arguments": {
                    "state_id": "s-0123456789abcdef",
                    "focus": {"x": 1, "y": 2},
                    "action": {"type": "press", "key": "Alt+Tab"},
                },
            },
        }),
    )
    .await;
    let unsupported = receive_json(&mut client).await;
    assert_eq!(unsupported["id"], 32);
    let result = &unsupported["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["code"], "unsupported_action");
    assert_eq!(result["structuredContent"]["outcome"], "not_started");
    assert_eq!(result["structuredContent"]["retryable"], false);
    let recovery = result["structuredContent"]["recovery"].as_str().unwrap();
    assert!(recovery.contains("No input was dispatched"));
    assert!(recovery.contains("launch_application"));
    assert_eq!(runtime.calls(), expected_tool_calls());

    runtime.fail_next();
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 33,
            "method": "tools/call",
            "params": {"name": "list_applications", "arguments": {"scope": "running"}},
        }),
    )
    .await;
    let runtime_error = receive_json(&mut client).await;
    assert_eq!(runtime_error["id"], 33);
    assert_eq!(
        runtime_error["result"],
        json!({
            "content": [{"type": "text", "text": RUNTIME_ERROR_TEXT}],
            "isError": true,
            "structuredContent": runtime_error_structured_content(),
        })
    );
    assert!(runtime_error.get("error").is_none());

    stop_server(client, server, &runtime).await;
}

#[tokio::test]
async fn structured_content_is_gated_by_protocol_version_at_one_return_point() {
    for (requested, negotiated, expects_structured_content) in [
        ("2024-11-05", "2024-11-05", false),
        ("2025-03-26", "2025-03-26", false),
        ("2025-06-18", "2025-06-18", true),
        ("2025-11-25", "2025-11-25", true),
        ("2026-07-28", "2026-07-28", true),
        ("2099-01-01", "2025-11-25", true),
    ] {
        let (listed, success, response) = results_for_protocol(requested, negotiated).await;
        for tool in listed["result"]["tools"].as_array().expect("listed tools") {
            assert_eq!(
                tool.get("outputSchema").is_some(),
                expects_structured_content,
                "output schema for protocol {negotiated}"
            );
        }
        assert_eq!(
            success["result"].get("structuredContent"),
            expects_structured_content.then_some(&json!({"status": "fake"})),
            "successful result for protocol {negotiated}"
        );
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .expect("runtime error text");

        assert_eq!(text, RUNTIME_ERROR_TEXT, "protocol {negotiated}");
        assert_eq!(result["isError"], true, "protocol {negotiated}");
        assert_eq!(
            result.get("structuredContent"),
            expects_structured_content.then_some(&runtime_error_structured_content()),
            "protocol {negotiated}"
        );
        assert!(response.get("error").is_none(), "protocol {negotiated}");
    }
}

async fn results_for_protocol(requested: &str, negotiated: &str) -> (Value, Value, Value) {
    let runtime = FakeRuntime::new(STANDARD.encode(test_png()));
    let (client_transport, server) = spawn_server(&runtime);
    let mut client = IntoTransport::<RoleClient, _, _>::into_transport(client_transport);

    let initialized = initialize(&mut client, requested, "readiness-protocol-test").await;
    assert_eq!(
        initialized["result"]["protocolVersion"], negotiated,
        "protocol negotiation"
    );
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/list",
        }),
    )
    .await;
    let listed = receive_json(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "list_applications", "arguments": {"scope": "running"}},
        }),
    )
    .await;
    let success = receive_json(&mut client).await;
    assert_eq!(success["id"], 2);

    runtime.fail_next();
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "list_applications", "arguments": {"scope": "running"}},
        }),
    )
    .await;
    let response = receive_json(&mut client).await;
    assert_eq!(response["id"], 3);

    stop_server(client, server, &runtime).await;
    (listed, success, response)
}

fn invalid_arguments_structured_content() -> Value {
    json!({
        "code": "invalid_arguments",
        "message": "missing required argument \"type\"",
        "outcome": "not_started",
        "retryable": true,
        "recovery": "Correct the arguments using the tool input schema, then retry.",
    })
}

fn runtime_error_structured_content() -> Value {
    json!({
        "code": "fake_runtime_unavailable",
        "message": "fake runtime unavailable",
        "outcome": "not_started",
        "retryable": true,
        "recovery": "Call observe for current state, then retry only if the requested action is still needed.",
    })
}

fn valid_tool_calls() -> Vec<(&'static str, Value)> {
    vec![
        ("list_applications", json!({"scope": "running"})),
        (
            "launch_application",
            json!({"desktop_id": "org.example.Music.desktop"}),
        ),
        (
            "observe",
            json!({
                "target": " Editor ",
                "view": "visible",
                "query": " button ",
            }),
        ),
        (
            "act_on_element",
            json!({
                "state_id": "s-0123456789abcdef",
                "element_id": "007",
                "action": {"type": "named", "name": "menu"},
            }),
        ),
        (
            "pointer",
            json!({
                "state_id": "s-0123456789abcdef",
                "action": {"type": "scroll", "x": 10.5, "y": 20.25, "direction": "right", "steps": 3},
            }),
        ),
        (
            "keyboard",
            json!({
                "state_id": "s-0123456789abcdef",
                "focus": {"element_id": "7"},
                "action": {"type": "type", "text": "hello\nworld"},
            }),
        ),
    ]
}

fn expected_tool_calls() -> Vec<ToolCall> {
    vec![
        ToolCall::ListApplications {
            scope: ApplicationScope::Running,
        },
        ToolCall::LaunchApplication {
            desktop_id: "org.example.Music.desktop".to_owned(),
        },
        ToolCall::Observe {
            target: "Editor".to_owned(),
            view: ObservationView::Visible,
            query: Some("button".to_owned()),
            text_limit: None,
            max_tree_nodes: None,
            max_tree_depth: None,
        },
        ToolCall::ActOnElement {
            state_id: "s-0123456789abcdef".to_owned(),
            element_id: "007".to_owned(),
            action: ElementAction::Named("menu".to_owned()),
        },
        ToolCall::Pointer {
            state_id: "s-0123456789abcdef".to_owned(),
            action: PointerAction::Scroll {
                x: 10.5,
                y: 20.25,
                delta_x: 360,
                delta_y: 0,
            },
        },
        ToolCall::Keyboard {
            state_id: "s-0123456789abcdef".to_owned(),
            focus: KeyboardFocus::Element("7".to_owned()),
            action: KeyboardAction::Type("hello\nworld".to_owned()),
        },
    ]
}

fn assert_success(response: &Value, name: &str) {
    assert!(response.get("error").is_none(), "{name}: {response}");
    assert_eq!(response["result"]["isError"], false, "{name}: {response}");
    assert_eq!(
        response["result"]["content"][0],
        json!({"type": "text", "text": "fake runtime success"}),
        "{name}: {response}"
    );
    assert_eq!(
        response["result"]["structuredContent"],
        json!({"status": "fake"}),
        "{name}: {response}"
    );
}

fn assert_png_content(response: &Value, expected_png: &[u8]) {
    let content = response["result"]["content"]
        .as_array()
        .expect("standard MCP content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    let decoded = STANDARD
        .decode(content[1]["data"].as_str().expect("base64 image data"))
        .expect("decode image content");
    assert_eq!(decoded, expected_png);
    let image = image::load_from_memory_with_format(&decoded, ImageFormat::Png)
        .expect("decode generated PNG");
    assert_eq!(image.dimensions(), (3, 2));
}

fn test_png() -> Vec<u8> {
    let pixels = [
        255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0, 255, 0, 255, 0, 255, 255,
    ];
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&pixels, 3, 2, ColorType::Rgb8.into())
        .expect("encode test PNG");
    png
}

fn message(value: Value) -> ClientJsonRpcMessage {
    serde_json::from_value(value).expect("valid client message")
}

fn spawn_server(runtime: &FakeRuntime) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>) {
    let runtime = runtime.clone();
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server = tokio::spawn(async move {
        let service = ComputerUseMcpServer::new(Arc::new(runtime.clone()))
            .serve(server_transport)
            .await
            .expect("initialize server");
        let waiting = service.waiting().await;
        let shutdown = runtime.shutdown().await;
        waiting.expect("wait for server");
        shutdown.expect("shut down fake runtime");
    });
    (client_transport, server)
}

async fn stop_server(
    client: impl Transport<RoleClient>,
    server: tokio::task::JoinHandle<()>,
    runtime: &FakeRuntime,
) {
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server should stop when transport closes")
        .expect("join server");
    assert_eq!(runtime.shutdowns(), 1);
}

async fn send_json(client: &mut impl Transport<RoleClient>, value: Value) {
    client.send(message(value)).await.expect("send request");
}

async fn initialize(
    client: &mut impl Transport<RoleClient>,
    protocol_version: &str,
    client_name: &str,
) -> Value {
    send_json(
        client,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": client_name, "version": "0.0.0"},
            },
        }),
    )
    .await;
    let response = receive_json(client).await;
    send_json(
        client,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    response
}

async fn receive_json(client: &mut impl Transport<RoleClient>) -> Value {
    response_value(client.receive().await.expect("receive response"))
}

fn response_value(message: ServerJsonRpcMessage) -> Value {
    serde_json::to_value(message).expect("serialize server message")
}
