use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
        ToolsCapability,
    },
    service::{RequestContext, RoleServer},
};

use crate::{
    VERSION,
    accessibility::{RuntimeConfig, SemanticRuntime},
    atspi_adapter::AtspiAdapter,
    contract::{SERVER_INSTRUCTIONS, tool_definition, tool_definitions},
    errors::{CliError, RuntimeError, ToolOutcome},
    runtime::{
        ActionProgress, CleanupStatus, DesktopRuntime, tool_error_result, with_action_progress,
    },
    screenshot::ProductionScreenshotCoordinator,
    validation::{ToolCall, validate_call},
    virtual_desktop::VirtualDesktopProvider,
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
        server_info()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_definitions().to_vec()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_definition(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let structured = supports_structured_content(&context);
        if tool_definition(&request.name).is_none() {
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
        Ok(for_protocol(
            self.execute(call, context.ct.cancelled()).await,
            structured,
        ))
    }
}

impl<R: DesktopRuntime> ComputerUseMcpServer<R> {
    pub(crate) async fn execute(
        &self,
        call: ToolCall,
        cancelled: impl std::future::Future<Output = ()> + Send,
    ) -> CallToolResult {
        tokio::pin!(cancelled);
        let progress = call
            .tracks_action()
            .then(|| Arc::new(ActionProgress::default()));
        // Initialization owns no action input. Wait before serializing calls
        // so list/launch can proceed while portal consent is pending.
        if call.requires_visual_session() {
            tokio::select! {
                biased;
                () = &mut cancelled => {
                    let mut result = tool_error_result(&cancelled_error(
                            progress.as_ref(),
                            "tool call cancelled while waiting for desktop initialization",
                    ));
                    result.structured_content.as_mut().expect("error has structured content")["session_replacement_required"] = serde_json::json!(true);
                    return result;
                }
                () = self.runtime.wait_for_desktop_session() => {}
            }
        }
        let _execution = tokio::select! {
            biased;
            () = &mut cancelled => {
                eprintln!("computer-use-mcp: queued tool call cancelled before execution");
                return tool_error_result(&cancelled_error(
                        progress.as_ref(),
                        "tool call cancelled before execution",
                    ));
            }
            guard = self.execution_barrier.lock() => guard,
        };
        // Some DesktopRuntime implementations enqueue work when execute() is
        // called, before polling its future. Check cancellation before even
        // constructing that future, not only as another select branch.
        tokio::select! {
            biased;
            () = &mut cancelled => return tool_error_result(&cancelled_error(progress.as_ref(), "tool call cancelled before execution")),
            () = std::future::ready(()) => {},
        }
        if self.unavailable.load(Ordering::Acquire) {
            tool_error_result(&RuntimeError::new(
                "backend_failed",
                "the desktop session was shut down after cancellation cleanup failed",
                ToolOutcome::NotStarted,
                false,
                "Disable and re-enable the MCP before issuing more computer-use calls.",
            ))
        } else {
            tokio::select! {
                biased;
                () = &mut cancelled => {
                    self.cancel_active(progress.as_ref()).await
                }
                result = self.runtime.execute(call, progress.clone()) => match result {
                    Ok(output) => output.into_mcp_result(),
                    Err(error) => {
                        eprintln!("computer-use-mcp: {error}");
                        tool_error_result(&error)
                    }
                },
            }
        }
    }

    async fn cancel_active(&self, progress: Option<&Arc<ActionProgress>>) -> CallToolResult {
        eprintln!("computer-use-mcp: tool call cancelled");
        self.cleanup_active_or_disable(progress).await;
        let error = cancelled_error(progress, "tool call cancelled while execution was active");
        tool_error_result(&error)
    }

    async fn cleanup_active_or_disable(&self, progress: Option<&Arc<ActionProgress>>) {
        let reason = match tokio::time::timeout(
            Duration::from_secs(8),
            self.runtime.cleanup(progress.cloned()),
        )
        .await
        {
            Ok(Ok(())) => return,
            Ok(Err(error)) => format!("cancellation cleanup failed: {error}"),
            Err(_) => "cancellation cleanup timed out".to_owned(),
        };
        self.disable_after_cleanup_failure(progress, reason).await;
    }

    async fn disable_after_cleanup_failure(
        &self,
        progress: Option<&Arc<ActionProgress>>,
        reason: impl std::fmt::Display,
    ) {
        if let Some(progress) = progress {
            progress.mark_cleanup_failed();
        }
        eprintln!("computer-use-mcp: {reason}; shutting down the desktop session");
        self.unavailable.store(true, Ordering::Release);
        shutdown_after_cleanup_failure(self.runtime.as_ref()).await;
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
    let snapshot = progress.snapshot();
    let outcome = snapshot.outcome();
    let (retryable, recovery) = if snapshot.cleanup == CleanupStatus::Failed {
        (
            false,
            "Cleanup failed or timed out; held input release and desktop restoration are not confirmed. Ask the user to restore the desktop and authorize an MCP restart, then observe before further actions.",
        )
    } else {
        match outcome {
            ToolOutcome::NotStarted => (true, "Retry the call if it is still needed."),
            ToolOutcome::Unknown => (
                false,
                "Input dispatch may have happened. Call observe and do not retry blindly.",
            ),
            ToolOutcome::Completed => (
                false,
                "Input dispatch completed. Call observe before deciding whether another action is needed.",
            ),
        }
    };
    with_action_progress(
        RuntimeError::new("cancelled", message, outcome, retryable, recovery),
        progress,
    )
}

pub async fn serve_worker_stdio() -> Result<(), CliError> {
    // Log the configured display for diagnostics. Environment settings alone
    // do not prove that portal, capture, input, and catalog share a compositor.
    crate::session::log_session_description();
    let runtime = production_runtime();
    eprintln!("computer-use-mcp: starting KDE desktop session initialization in the background");
    let result = crate::broker::worker_stdio(Arc::clone(&runtime)).await;
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
    // Live KWin awareness is production-only: tests construct
    // `SemanticRuntime` directly and keep the disabled provider, so no
    // session-bus traffic exists under test. Fail-closed downstream.
    let runtime = SemanticRuntime::with_screenshot_provider(
        AtspiAdapter::default(),
        ProductionScreenshotCoordinator::default(),
        RuntimeConfig::default(),
    )
    .with_virtual_desktop_provider(VirtualDesktopProvider::live());
    let runtime = Arc::new(runtime);
    // Track physical activity between calls too, so a foreground mutation
    // cannot resume until the quiet period has passed.
    runtime.arm_hardware_watcher();
    runtime
}

pub(crate) fn server_info() -> ServerInfo {
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

pub(crate) fn for_protocol(mut result: CallToolResult, structured: bool) -> CallToolResult {
    crate::runtime::take_element_ids(&mut result);
    if !structured {
        result.structured_content = None;
    }
    result
}

pub(crate) fn supports_structured_content(context: &RequestContext<RoleServer>) -> bool {
    context.protocol_version().is_some_and(|version| {
        version == ProtocolVersion::V_2025_06_18
            || version == ProtocolVersion::V_2025_11_25
            || version == ProtocolVersion::V_2026_07_28
    })
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn public_protocol_never_exposes_worker_identity_metadata() {
        let mut output = crate::runtime::ToolOutput::text("observation");
        output.element_ids.push("e-0000000000000001".into());
        let result = output.into_mcp_result();
        assert!(result.meta.is_some());
        for structured in [false, true] {
            assert!(for_protocol(result.clone(), structured).meta.is_none());
        }
    }

    #[test]
    fn cancellation_preserves_dispatch_outcome_even_when_cleanup_fails() {
        for expected in [
            ToolOutcome::NotStarted,
            ToolOutcome::Unknown,
            ToolOutcome::Completed,
        ] {
            let progress = Arc::new(ActionProgress::default());
            if expected != ToolOutcome::NotStarted {
                progress.mark_started();
            }
            if expected == ToolOutcome::Completed {
                progress.mark_completed();
            }
            let error = cancelled_error(Some(&progress), "interrupted");
            assert_eq!(error.outcome, expected);
            assert_eq!(error.retryable, expected == ToolOutcome::NotStarted);

            progress.mark_cleanup_failed();
            let error = cancelled_error(Some(&progress), "interrupted");
            assert_eq!(error.outcome, expected);
            assert!(!error.retryable);
            assert!(error.recovery.contains("not confirmed"));
            assert!(error.recovery.contains("restart"));
            assert_eq!(error.action_progress.unwrap()["cleanup"], "failed");
        }
    }
}
