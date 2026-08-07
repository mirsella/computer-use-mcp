use std::future::Future;

use rmcp::model::{CallToolResult, ContentBlock};

use crate::{errors::RuntimeError, validation::ToolCall};

pub trait DesktopRuntime: Send + Sync + 'static {
    fn execute(
        &self,
        call: ToolCall,
    ) -> impl Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_;
    fn cleanup(&self) -> impl Future<Output = Result<(), RuntimeError>> + Send + '_;
    fn shutdown(&self) -> impl Future<Output = Result<(), RuntimeError>> + Send + '_ {
        async move { self.cleanup().await }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub png_base64: Option<String>,
    pub structured_content: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            png_base64: None,
            structured_content: None,
        }
    }

    pub fn with_png_base64(mut self, png_base64: impl Into<String>) -> Self {
        self.png_base64 = Some(png_base64.into());
        self
    }

    pub fn with_structured_content(mut self, value: serde_json::Value) -> Self {
        self.structured_content = Some(value);
        self
    }

    pub fn into_mcp_result(self) -> CallToolResult {
        let mut content = vec![ContentBlock::text(self.text)];
        if let Some(data) = self.png_base64 {
            content.push(ContentBlock::image(data, "image/png"));
        }
        let mut result = CallToolResult::success(content);
        result.structured_content = self.structured_content;
        result
    }
}

pub fn tool_error_result(error: &RuntimeError) -> CallToolResult {
    let text = format!(
        "{}\nCode: {}\nOutcome: {}\nRetryable: {}\nRecovery: {}",
        error.message,
        error.code,
        error.outcome.as_str(),
        error.retryable,
        error.recovery
    );
    let mut result = CallToolResult::error(vec![ContentBlock::text(text)]);
    result.structured_content = Some(serde_json::json!({
        "code": error.code,
        "message": error.message,
        "outcome": error.outcome.as_str(),
        "retryable": error.retryable,
        "recovery": error.recovery,
    }));
    result
}
