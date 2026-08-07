use std::{
    future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use computer_use_mcp::{
    errors::RuntimeError,
    runtime::{DesktopRuntime, ToolOutput},
    server::ComputerUseMcpServer,
    validation::ToolCall,
};
use rmcp::{
    RoleClient, ServiceExt,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{IntoTransport, Transport},
};
use serde_json::{Value, json};

struct BlockingRuntime {
    calls: Arc<AtomicUsize>,
    cleanup_complete: Arc<AtomicBool>,
}

struct QueuedRuntime {
    calls: Arc<AtomicUsize>,
    release_first: Arc<tokio::sync::Notify>,
}

struct CleanupFailureRuntime {
    calls: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}

impl DesktopRuntime for QueuedRuntime {
    fn execute(
        &self,
        _call: ToolCall,
    ) -> impl future::Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_ {
        let first = self.calls.fetch_add(1, Ordering::AcqRel) == 0;
        async move {
            if first {
                self.release_first.notified().await;
            }
            Ok(ToolOutput::text("executed"))
        }
    }

    async fn cleanup(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

impl DesktopRuntime for BlockingRuntime {
    fn execute(
        &self,
        _call: ToolCall,
    ) -> impl future::Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_ {
        let first = self.calls.fetch_add(1, Ordering::AcqRel) == 0;
        let clean = self.cleanup_complete.load(Ordering::Acquire);
        async move {
            if first {
                future::pending().await
            } else {
                if !clean {
                    return Err(RuntimeError::not_started(
                        "cleanup_incomplete",
                        "mutation started before cleanup barrier",
                    ));
                }
                Ok(ToolOutput::text("after cleanup"))
            }
        }
    }

    async fn cleanup(&self) -> Result<(), RuntimeError> {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.cleanup_complete.store(true, Ordering::Release);
        Ok(())
    }
}

impl DesktopRuntime for CleanupFailureRuntime {
    fn execute(
        &self,
        _call: ToolCall,
    ) -> impl future::Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_ {
        let first = self.calls.fetch_add(1, Ordering::AcqRel) == 0;
        async move {
            if first {
                future::pending().await
            } else if self.shutdowns.load(Ordering::Acquire) == 0 {
                Err(RuntimeError::not_started(
                    "shutdown_incomplete",
                    "call started before failed cleanup forced shutdown",
                ))
            } else {
                Ok(ToolOutput::text("after shutdown"))
            }
        }
    }

    async fn cleanup(&self) -> Result<(), RuntimeError> {
        Err(RuntimeError::not_started(
            "cleanup_failed",
            "planned cleanup failure",
        ))
    }

    async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.shutdowns.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[tokio::test]
async fn cancelled_runtime_call_emits_no_response_and_server_continues() {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let cleanup_complete = Arc::new(AtomicBool::new(false));
    let server_cleanup = Arc::clone(&cleanup_complete);
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        let service = ComputerUseMcpServer::new(Arc::new(BlockingRuntime {
            calls: server_calls,
            cleanup_complete: server_cleanup,
        }))
        .serve(server_transport)
        .await
        .expect("initialize server");
        service.waiting().await.expect("wait for server");
    });
    let mut client = IntoTransport::<RoleClient, _, _>::into_transport(client_transport);

    client
        .send(message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "cancellation-test", "version": "0.0.0"},
            },
        })))
        .await
        .expect("send initialize");
    assert_eq!(
        response_id(client.receive().await.expect("initialize response")),
        Some(1)
    );

    for payload in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "keyboard", "arguments": keyboard_arguments()},
        }),
    ] {
        client.send(message(payload)).await.expect("send message");
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first mutation should start before cancellation");

    for payload in [
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 2, "reason": "test"},
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "keyboard", "arguments": keyboard_arguments()},
        }),
    ] {
        client.send(message(payload)).await.expect("send message");
    }

    let response = client.receive().await.expect("next call response");
    assert_eq!(response_id(response), Some(3));
    assert!(cleanup_complete.load(Ordering::Acquire));
    drop(client);
    server.await.expect("join server");
}

#[tokio::test]
async fn failed_cancellation_cleanup_forces_shutdown_before_next_call() {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let calls = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let server_calls = Arc::clone(&calls);
    let server_shutdowns = Arc::clone(&shutdowns);
    let server = tokio::spawn(async move {
        let service = ComputerUseMcpServer::new(Arc::new(CleanupFailureRuntime {
            calls: server_calls,
            shutdowns: server_shutdowns,
        }))
        .serve(server_transport)
        .await
        .expect("initialize server");
        service.waiting().await.expect("wait for server");
    });
    let mut client = IntoTransport::<RoleClient, _, _>::into_transport(client_transport);

    client
        .send(message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "cleanup-failure-test", "version": "0.0.0"},
            },
        })))
        .await
        .unwrap();
    assert_eq!(response_id(client.receive().await.unwrap()), Some(1));
    for payload in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        tool_call(2),
    ] {
        client.send(message(payload)).await.unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first call should start");

    client
        .send(message(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 2, "reason": "test cleanup failure"},
        })))
        .await
        .unwrap();
    client.send(message(tool_call(3))).await.unwrap();
    let response = client.receive().await.unwrap();
    assert_eq!(response_id(response.clone()), Some(3));
    let value = serde_json::to_value(response).unwrap();
    assert_eq!(value["result"]["isError"], true);
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(shutdowns.load(Ordering::Acquire), 1);

    drop(client);
    server.await.unwrap();
}

#[tokio::test]
async fn read_only_call_cancelled_while_queued_never_executes() {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let calls = Arc::new(AtomicUsize::new(0));
    let release_first = Arc::new(tokio::sync::Notify::new());
    let server_calls = Arc::clone(&calls);
    let server_release = Arc::clone(&release_first);
    let server = tokio::spawn(async move {
        let service = ComputerUseMcpServer::new(Arc::new(QueuedRuntime {
            calls: server_calls,
            release_first: server_release,
        }))
        .serve(server_transport)
        .await
        .expect("initialize server");
        service.waiting().await.expect("wait for server");
    });
    let mut client = IntoTransport::<RoleClient, _, _>::into_transport(client_transport);

    client
        .send(message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "queued-cancellation-test", "version": "0.0.0"},
            },
        })))
        .await
        .unwrap();
    assert_eq!(response_id(client.receive().await.unwrap()), Some(1));
    for payload in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        tool_call(2),
    ] {
        client.send(message(payload)).await.unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first mutation should start");

    client.send(message(list_call(3))).await.unwrap();
    client
        .send(message(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 3, "reason": "queued test"},
        })))
        .await
        .unwrap();
    release_first.notify_one();
    assert_eq!(response_id(client.receive().await.unwrap()), Some(2));

    client.send(message(tool_call(4))).await.unwrap();
    assert_eq!(response_id(client.receive().await.unwrap()), Some(4));
    assert_eq!(calls.load(Ordering::Acquire), 2);
    drop(client);
    server.await.unwrap();
}

fn tool_call(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": "keyboard", "arguments": keyboard_arguments()},
    })
}

fn list_call(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": "list_applications", "arguments": {"scope": "running"}},
    })
}

fn keyboard_arguments() -> Value {
    json!({
        "state_id": "s-0000000000000000",
        "focus": {"x": 10, "y": 20},
        "action": {"type": "press", "key": "Return"},
    })
}

fn message(value: Value) -> ClientJsonRpcMessage {
    serde_json::from_value(value).expect("valid client message")
}

fn response_id(message: ServerJsonRpcMessage) -> Option<u64> {
    serde_json::to_value(message).expect("serialize server message")["id"].as_u64()
}
