use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
        ToolsCapability,
    },
    service::{RequestContext, RoleServer, ServerInitializeError},
};

use crate::{
    VERSION,
    accessibility::{RuntimeConfig, SemanticRuntime},
    atspi_adapter::AtspiAdapter,
    contract::{SERVER_INSTRUCTIONS, TOOL_NAMES, tool_definitions},
    errors::{CliError, RuntimeError, ToolOutcome},
    runtime::{ActionProgress, DesktopRuntime, tool_error_result, with_action_progress},
    screenshot::ProductionScreenshotCoordinator,
    validation::validate_call,
};

#[derive(Debug)]
pub struct ComputerUseMcpServer<R = SemanticRuntime<AtspiAdapter, ProductionScreenshotCoordinator>>
{
    runtime: Arc<R>,
    execution_barrier: tokio::sync::Mutex<()>,
    unavailable: AtomicBool,
}

impl<R: DesktopRuntime> ComputerUseMcpServer<R> {
    pub fn new(runtime: Arc<R>) -> Self {
        runtime.start();
        Self {
            runtime,
            execution_barrier: tokio::sync::Mutex::new(()),
            unavailable: AtomicBool::new(false),
        }
    }
}

impl<R: DesktopRuntime> ServerHandler for ComputerUseMcpServer<R> {
    fn get_info(&self) -> ServerInfo {
        let mut tools = ToolsCapability::default();
        tools.list_changed = Some(false);
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools_with(tools)
                .build(),
        )
        .with_server_info(Implementation::new("computer-use-mcp", VERSION))
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_definitions()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_definitions()
            .into_iter()
            .find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let structured = supports_structured_content(&context);
        if !TOOL_NAMES.contains(&request.name.as_ref()) {
            return Err(McpError::invalid_params(
                format!("unknown tool {:?}", request.name),
                None,
            ));
        }
        let arguments = request.arguments.unwrap_or_default();
        let call = match validate_call(&request.name, arguments) {
            Ok(call) => call,
            Err(error) => {
                return Ok(for_protocol(tool_error_result(&error), structured));
            }
        };
        let progress = call
            .tracks_action()
            .then(|| Arc::new(ActionProgress::default()));
        if call.requires_visual_session() {
            tokio::select! {
                () = self.runtime.wait_for_desktop_session() => {}
                () = context.ct.cancelled() => {
                    eprintln!("computer-use-mcp: tool call cancelled while waiting for desktop session initialization");
                    return Ok(for_protocol(
                        tool_error_result(&cancelled_error(progress.as_ref(), "tool call cancelled before execution")),
                        structured,
                    ));
                }
            }
        }
        let _execution = tokio::select! {
            guard = self.execution_barrier.lock() => guard,
            () = context.ct.cancelled() => {
                eprintln!("computer-use-mcp: queued tool call cancelled before execution");
                return Ok(for_protocol(
                    tool_error_result(&cancelled_error(
                        progress.as_ref(),
                        "tool call cancelled before execution",
                    )),
                    structured,
                ));
            }
        };
        let result = if self.unavailable.load(Ordering::Acquire) {
            tool_error_result(&RuntimeError::new(
                "backend_failed",
                "the desktop session was shut down after cancellation cleanup failed",
                ToolOutcome::NotStarted,
                false,
                "Disable and re-enable the MCP before issuing more computer-use calls.",
            ))
        } else if context.ct.is_cancelled() {
            eprintln!("computer-use-mcp: queued tool call cancelled before execution");
            tool_error_result(&cancelled_error(
                progress.as_ref(),
                "tool call cancelled before execution",
            ))
        } else {
            tokio::select! {
                result = self.runtime.execute(call, progress.clone()) => match result {
                    Ok(output) => output.into_mcp_result(),
                    Err(error) => {
                        eprintln!("computer-use-mcp: {error}");
                        tool_error_result(&error)
                    }
                },
                () = context.ct.cancelled() => {
                    eprintln!("computer-use-mcp: tool call cancelled");
                         match tokio::time::timeout(Duration::from_secs(2), self.runtime.cleanup(progress.clone())).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            eprintln!("computer-use-mcp: cancellation cleanup failed: {error}; shutting down the desktop session");
                            self.unavailable.store(true, Ordering::Release);
                            shutdown_after_cleanup_failure(self.runtime.as_ref()).await;
                        }
                        Err(_) => {
                            eprintln!("computer-use-mcp: cancellation cleanup timed out; shutting down the desktop session");
                            self.unavailable.store(true, Ordering::Release);
                            shutdown_after_cleanup_failure(self.runtime.as_ref()).await;
                        }
                    }
                    let error = cancelled_error(
                        progress.as_ref(),
                        "tool call cancelled while execution was active",
                    );
                    tool_error_result(&error)
                }
            }
        };
        Ok(for_protocol(result, structured))
    }
}

async fn shutdown_after_cleanup_failure<R: DesktopRuntime>(runtime: &R) {
    match tokio::time::timeout(Duration::from_secs(6), runtime.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("computer-use-mcp: cancellation shutdown failed: {error}"),
        Err(_) => eprintln!("computer-use-mcp: cancellation shutdown timed out"),
    }
}

fn cancelled_error(progress: Option<&Arc<ActionProgress>>, message: &str) -> RuntimeError {
    let Some(progress) = progress else {
        return RuntimeError::new(
            "cancelled",
            message,
            ToolOutcome::NotStarted,
            true,
            "Retry the call if it is still needed.",
        );
    };
    let outcome = progress.snapshot().outcome();
    let (retryable, recovery) = match outcome {
        ToolOutcome::NotStarted => (true, "Retry the call if it is still needed."),
        ToolOutcome::Unknown => (
            false,
            "Input dispatch may have happened. Call observe and do not retry blindly.",
        ),
        ToolOutcome::Completed => (
            false,
            "Input dispatch completed. Call observe before deciding whether another action is needed.",
        ),
    };
    with_action_progress(
        RuntimeError::new("cancelled", message, outcome, retryable, recovery),
        progress,
    )
}

pub async fn serve_stdio() -> Result<(), CliError> {
    let runtime = production_runtime();
    eprintln!("computer-use-mcp: starting KDE desktop session initialization in the background");
    let result = async {
        let service = match ComputerUseMcpServer::new(Arc::clone(&runtime))
            .serve(rmcp::transport::stdio())
            .await
        {
            Ok(service) => service,
            // An MCP host may close stdin while starting or stopping the child.
            // No request was accepted, so this is a clean shutdown rather than a server failure.
            Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
            Err(error) => {
                return Err(CliError::Mcp(format!(
                    "failed to start MCP stdio server: {error}"
                )));
            }
        };
        service.waiting().await.map(|_| ()).map_err(|error| {
            CliError::Mcp(format!("MCP stdio server stopped with an error: {error}"))
        })
    }
    .await;
    let shutdown = runtime
        .shutdown()
        .await
        .map_err(|error| CliError::Mcp(format!("shutdown cleanup failed: {error}")));
    match (result, shutdown) {
        (Err(error), Err(shutdown)) => {
            eprintln!("computer-use-mcp: shutdown also failed after server error: {shutdown}");
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), shutdown) => shutdown,
    }
}

pub fn production_runtime() -> Arc<SemanticRuntime<AtspiAdapter, ProductionScreenshotCoordinator>> {
    Arc::new(SemanticRuntime::with_screenshot_provider(
        AtspiAdapter::default(),
        ProductionScreenshotCoordinator::default(),
        RuntimeConfig::default(),
    ))
}

fn for_protocol(mut result: CallToolResult, structured: bool) -> CallToolResult {
    if !structured {
        result.structured_content = None;
    }
    result
}

fn supports_structured_content(context: &RequestContext<RoleServer>) -> bool {
    context.protocol_version().is_some_and(|version| {
        version == ProtocolVersion::V_2025_06_18
            || version == ProtocolVersion::V_2025_11_25
            || version == ProtocolVersion::V_2026_07_28
    })
}
