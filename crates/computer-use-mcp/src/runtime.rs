use std::{
    future::Future,
    sync::atomic::{AtomicU8, Ordering},
};

use rmcp::model::{CallToolResult, ContentBlock};

use crate::{
    errors::{RuntimeError, ToolOutcome},
    validation::ToolCall,
};

pub const MAX_MODEL_TEXT_BYTES: usize = 16_000;
pub const MAX_MODEL_STRUCTURED_BYTES: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStage {
    NotStarted,
    Started,
    Completed,
}

impl DispatchStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Started => "started",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStatus {
    NotNeeded,
    Completed,
    Failed,
}

impl CleanupStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotNeeded => "not_needed",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostStatus {
    NotRun,
    NotRequested,
    Observed,
    Timeout,
    StreamDegraded,
    SessionUnavailable,
    CaptureFailed,
    CatalogRefreshFailed,
    AccessibilityRefreshFailed,
    Unavailable,
}

impl PostStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRun => "not_run",
            Self::NotRequested => "not_requested",
            Self::Observed => "observed",
            Self::Timeout => "timeout",
            Self::StreamDegraded => "stream_degraded",
            Self::SessionUnavailable => "session_unavailable",
            Self::CaptureFailed => "capture_failed",
            Self::CatalogRefreshFailed => "catalog_refresh_failed",
            Self::AccessibilityRefreshFailed => "accessibility_refresh_failed",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionProgressSnapshot {
    pub dispatch_stage: DispatchStage,
    pub cleanup: CleanupStatus,
    pub post_visual: PostStatus,
    pub post_accessibility: PostStatus,
}

impl ActionProgressSnapshot {
    pub const fn outcome(self) -> ToolOutcome {
        match self.dispatch_stage {
            DispatchStage::NotStarted => ToolOutcome::NotStarted,
            DispatchStage::Started => ToolOutcome::Unknown,
            DispatchStage::Completed => ToolOutcome::Completed,
        }
    }
}

/// The single mutable attempt record shared by the server boundary, runtime,
/// generated input, cleanup, and cancellation reporting.
#[derive(Debug)]
pub struct ActionProgress {
    dispatch_stage: AtomicU8,
    cleanup: AtomicU8,
    post_visual: AtomicU8,
    post_accessibility: AtomicU8,
}

impl Default for ActionProgress {
    fn default() -> Self {
        Self {
            dispatch_stage: AtomicU8::new(0),
            cleanup: AtomicU8::new(0),
            post_visual: AtomicU8::new(0),
            post_accessibility: AtomicU8::new(0),
        }
    }
}

impl ActionProgress {
    pub fn snapshot(&self) -> ActionProgressSnapshot {
        ActionProgressSnapshot {
            dispatch_stage: match self.dispatch_stage.load(Ordering::Acquire) {
                0 => DispatchStage::NotStarted,
                1 => DispatchStage::Started,
                2 => DispatchStage::Completed,
                _ => {
                    eprintln!("computer-use-mcp: invalid dispatch progress state");
                    DispatchStage::Started
                }
            },
            cleanup: match self.cleanup.load(Ordering::Acquire) {
                0 => CleanupStatus::NotNeeded,
                1 => CleanupStatus::Completed,
                2 => CleanupStatus::Failed,
                _ => {
                    eprintln!("computer-use-mcp: invalid cleanup progress state");
                    CleanupStatus::Failed
                }
            },
            post_visual: post_status(self.post_visual.load(Ordering::Acquire)),
            post_accessibility: post_status(self.post_accessibility.load(Ordering::Acquire)),
        }
    }

    pub fn mark_started(&self) {
        match self
            .dispatch_stage
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            // An attempt can include both semantic focus and generated input.
            Ok(_) | Err(1) => {}
            Err(_) => eprintln!("computer-use-mcp: refusing to regress dispatch start"),
        }
    }

    pub fn mark_completed(&self) {
        if self
            .dispatch_stage
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            eprintln!("computer-use-mcp: dispatch completed without a started stage");
        }
    }

    pub fn mark_cleanup_completed(&self) {
        if self
            .cleanup
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
            && self.cleanup.load(Ordering::Acquire) != 1
        {
            eprintln!("computer-use-mcp: cleanup completion could not replace terminal failure");
        }
    }

    pub fn mark_cleanup_failed(&self) {
        self.cleanup.store(2, Ordering::Release);
    }

    pub fn mark_post_visual(&self, status: PostStatus) {
        self.post_visual.store(status as u8, Ordering::Release);
    }

    pub fn mark_post_accessibility(&self, status: PostStatus) {
        self.post_accessibility
            .store(status as u8, Ordering::Release);
    }
}

fn post_status(value: u8) -> PostStatus {
    match value {
        0 => PostStatus::NotRun,
        1 => PostStatus::NotRequested,
        2 => PostStatus::Observed,
        3 => PostStatus::Timeout,
        4 => PostStatus::StreamDegraded,
        5 => PostStatus::SessionUnavailable,
        6 => PostStatus::CaptureFailed,
        7 => PostStatus::CatalogRefreshFailed,
        8 => PostStatus::AccessibilityRefreshFailed,
        9 => PostStatus::Unavailable,
        _ => {
            eprintln!("computer-use-mcp: invalid post-action progress state");
            PostStatus::Unavailable
        }
    }
}

pub fn action_progress_json(progress: ActionProgressSnapshot) -> serde_json::Value {
    serde_json::json!({
        "dispatch_stage": progress.dispatch_stage.as_str(),
        "cleanup": progress.cleanup.as_str(),
        "post_visual": progress.post_visual.as_str(),
        "post_accessibility": progress.post_accessibility.as_str(),
    })
}

pub fn compact_action_progress(progress: ActionProgressSnapshot) -> String {
    format!(
        "Action progress: dispatch={} cleanup={} visual={} accessibility={}",
        progress.dispatch_stage.as_str(),
        progress.cleanup.as_str(),
        progress.post_visual.as_str(),
        progress.post_accessibility.as_str(),
    )
}

pub fn with_action_progress(error: RuntimeError, progress: &ActionProgress) -> RuntimeError {
    with_action_progress_snapshot(error, progress.snapshot())
}

pub fn with_action_progress_snapshot(
    mut error: RuntimeError,
    snapshot: ActionProgressSnapshot,
) -> RuntimeError {
    let recovery = error
        .recovery
        .split_once(" Action progress:")
        .map_or(error.recovery.as_str(), |(recovery, _)| recovery);
    error.recovery = format!("{recovery} {}", compact_action_progress(snapshot));
    error.action_progress = Some(action_progress_json(snapshot));
    error
}

pub trait DesktopRuntime: Send + Sync + 'static {
    fn start(&self) {}
    /// True after approval failure or capture exhaustion requires a new owner.
    /// This must not include a user-takeover latch or an ordinary action error.
    fn desktop_session_exhausted(&self) -> bool {
        false
    }
    fn wait_for_desktop_session(&self) -> impl Future<Output = ()> + Send + '_ {
        async {}
    }
    fn execute(
        &self,
        call: ToolCall,
        progress: Option<std::sync::Arc<ActionProgress>>,
    ) -> impl Future<Output = Result<ToolOutput, RuntimeError>> + Send + '_;
    fn cleanup(
        &self,
        progress: Option<std::sync::Arc<ActionProgress>>,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send + '_;
    fn shutdown(&self) -> impl Future<Output = Result<(), RuntimeError>> + Send + '_ {
        async move { self.cleanup(None).await }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub png_base64: Option<String>,
    pub structured_content: Option<serde_json::Value>,
    // IDs emitted by the text renderer, independent of the bounded JSON projection.
    pub(crate) element_ids: Vec<String>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            png_base64: None,
            structured_content: None,
            element_ids: Vec::new(),
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

    pub fn with_action_progress(mut self, progress: &ActionProgress) -> Self {
        let snapshot = progress.snapshot();
        if !self
            .text
            .lines()
            .any(|line| line.starts_with("Action progress: "))
        {
            self.text.push('\n');
            self.text.push_str(&compact_action_progress(snapshot));
            self.text = bound_text_with_suffix("", &self.text, MAX_MODEL_TEXT_BYTES);
        }
        let evidence = action_progress_json(snapshot);
        self.structured_content = match self.structured_content.take() {
            Some(serde_json::Value::Object(mut object)) => {
                object.insert("action_progress".into(), evidence);
                Some(serde_json::Value::Object(object))
            }
            Some(value) => {
                eprintln!(
                    "computer-use-mcp: wrapping non-object structured content to attach action progress"
                );
                Some(serde_json::json!({
                    "value": value,
                    "action_progress": evidence,
                    "truncated": true,
                    "truncation_reason": "action_progress_wrapper"
                }))
            }
            None => Some(serde_json::json!({
                    "action_progress": evidence
            })),
        };
        self
    }

    /// Apply the final model-facing byte budgets after all optional evidence
    /// and progress fields have been attached.  Domain-specific projections
    /// should trim their own rich fields first; this last guard prevents a
    /// newly-added suffix or dynamic backend value from violating the public
    /// contract.
    fn bounded(mut self) -> Self {
        self.text = bound_text(self.text, MAX_MODEL_TEXT_BYTES);
        if let Some(structured) = self.structured_content.take() {
            self.structured_content =
                Some(bound_structured(structured, MAX_MODEL_STRUCTURED_BYTES));
        }
        self
    }

    pub fn into_mcp_result(self) -> CallToolResult {
        let bounded = self.bounded();
        let mut content = vec![ContentBlock::text(bounded.text)];
        if let Some(data) = bounded.png_base64 {
            content.push(ContentBlock::image(data, "image/png"));
        }
        let mut result = CallToolResult::success(content);
        result.structured_content = bounded.structured_content;
        if !bounded.element_ids.is_empty() {
            result.meta = Some(rmcp::model::Meta(serde_json::Map::from_iter([(
                ELEMENT_IDS_META.into(),
                serde_json::Value::from(bounded.element_ids),
            )])));
        }
        result
    }
}

const ELEMENT_IDS_META: &str = "computer-use-mcp/worker-element-ids";

/// Private worker routing data, removed at either public MCP boundary.
pub(crate) fn take_element_ids(result: &mut CallToolResult) -> Option<serde_json::Value> {
    let ids = result
        .meta
        .as_mut()
        .and_then(|meta| meta.0.remove(ELEMENT_IDS_META));
    if result.meta.as_ref().is_some_and(|meta| meta.0.is_empty()) {
        result.meta = None;
    }
    ids
}

pub fn tool_error_result(error: &RuntimeError) -> CallToolResult {
    let suffix = format!(
        "Code: {}\nOutcome: {}\nRetryable: {}\nRecovery: {}",
        error.code,
        error.outcome.as_str(),
        error.retryable,
        error.recovery
    );
    let text = bound_text_with_suffix(&error.message, &suffix, MAX_MODEL_TEXT_BYTES);
    let mut result = CallToolResult::error(vec![ContentBlock::text(text)]);
    let mut structured = serde_json::json!({
        "code": error.code,
        "message": error.message,
        "outcome": error.outcome.as_str(),
        "retryable": error.retryable,
        "recovery": error.recovery,
    });
    if let Some(action_progress) = &error.action_progress {
        structured["action_progress"] = action_progress.clone();
    }
    result.structured_content = Some(bound_structured(structured, MAX_MODEL_STRUCTURED_BYTES));
    result
}

/// Reserve routing evidence before projecting the worker payload. Error and
/// dispatch suffixes retain their existing priority within the smaller budget.
pub(crate) fn annotate_mcp_result(
    result: &mut CallToolResult,
    header: &str,
    metadata: serde_json::Map<String, serde_json::Value>,
) {
    let text_budget = MAX_MODEL_TEXT_BYTES
        .checked_sub(header.len())
        .expect("routing header exceeds text budget");
    for (index, content) in result.content.iter_mut().enumerate() {
        if let ContentBlock::Text(text) = content {
            let budget = if index == 0 {
                text_budget
            } else {
                MAX_MODEL_TEXT_BYTES
            };
            let suffix = text
                .text
                .rfind("\nCode:")
                .or_else(|| text.text.rfind("\nAction progress:"));
            text.text = if let Some(offset) = suffix {
                bound_text_with_suffix(&text.text[..offset], &text.text[offset + 1..], budget)
            } else {
                bound_text(std::mem::take(&mut text.text), budget)
            };
            if index == 0 {
                text.text.insert_str(0, header);
            }
        }
    }
    let overhead = serde_json::to_vec(&metadata)
        .expect("structured output metadata must be serializable")
        .len()
        .checked_sub(1)
        .expect("structured output metadata must serialize as an object");
    let budget = MAX_MODEL_STRUCTURED_BYTES
        .checked_sub(overhead)
        .expect("routing metadata exceeds structured budget");
    let mut structured = bound_structured(
        result
            .structured_content
            .take()
            .unwrap_or_else(|| serde_json::json!({})),
        budget,
    );
    structured
        .as_object_mut()
        .expect("desktop results have object structured content")
        .extend(metadata);
    result.structured_content = Some(structured);
}

const OUTPUT_TRUNCATION_MARKER: &str =
    "Truncated: reason=response_byte_budget value_projection=bounded\n";

fn bound_text(text: String, maximum: usize) -> String {
    if text.len() <= maximum {
        return text;
    }
    if OUTPUT_TRUNCATION_MARKER.len() >= maximum {
        return String::new();
    }
    let mut end = maximum - OUTPUT_TRUNCATION_MARKER.len();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end = text[..end]
        .rfind(['\n', ' ', '\t'])
        .map_or(0, |boundary| boundary + 1);
    let mut bounded = text[..end].to_owned();
    if !bounded.is_empty() && !bounded.ends_with('\n') {
        bounded.push('\n');
    }
    bounded.push_str(OUTPUT_TRUNCATION_MARKER);
    bounded
}

fn bound_text_with_suffix(prefix: &str, suffix: &str, maximum: usize) -> String {
    let separator = usize::from(!prefix.is_empty() && !prefix.ends_with('\n'));
    let suffix_bytes = suffix.len().saturating_add(separator);
    if suffix_bytes > maximum {
        eprintln!("computer-use-mcp: critical response suffix exceeds the model text byte budget");
        return bound_text(suffix.to_owned(), maximum);
    }
    if prefix.len().saturating_add(suffix_bytes) <= maximum {
        return format!("{prefix}{}{suffix}", if separator == 0 { "" } else { "\n" });
    }
    let prefix_budget = maximum.saturating_sub(suffix_bytes);
    let prefix = bound_text(prefix.to_owned(), prefix_budget);
    format!("{prefix}{}{suffix}", if separator == 0 { "" } else { "\n" })
}

fn bound_structured(value: serde_json::Value, maximum: usize) -> serde_json::Value {
    if json_size(&value) <= maximum {
        return value;
    }
    let mut bounded = serde_json::Map::new();
    bounded.insert("truncated".into(), serde_json::Value::Bool(true));
    bounded.insert(
        "truncation_reason".into(),
        serde_json::Value::String("response_byte_budget".into()),
    );

    let keys = [
        "desktop",
        "session_id",
        "code",
        "message",
        "outcome",
        "retryable",
        "recovery",
        "status",
        "target",
        "condition",
        "satisfied",
        "evidence",
        "action_progress",
        "replacement_observation_id",
        "replacement_frame_id",
        "replacement_element_id",
    ];
    if let serde_json::Value::Object(object) = &value {
        for key in keys.into_iter().chain(object.keys().map(String::as_str)) {
            if bounded.contains_key(key) {
                continue;
            }
            let Some(original) = object.get(key) else {
                continue;
            };
            let candidate = if keys.contains(&key) {
                original.clone()
            } else {
                compact_json(original, 1_024)
            };
            bounded.insert(key.to_owned(), candidate);
            if json_size(&serde_json::Value::Object(bounded.clone())) > maximum {
                bounded.remove(key);
            }
        }
    } else {
        let candidate = compact_json(
            &value,
            maximum.saturating_sub(json_size(&serde_json::Value::Object(bounded.clone()))),
        );
        bounded.insert("value".into(), candidate);
    }

    let bounded = serde_json::Value::Object(bounded);
    if json_size(&bounded) <= maximum {
        bounded
    } else {
        serde_json::json!({
            "truncated": true,
            "truncation_reason": "response_byte_budget"
        })
    }
}

fn compact_json(value: &serde_json::Value, maximum: usize) -> serde_json::Value {
    if json_size(value) <= maximum {
        return value.clone();
    }
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(bound_text(text.clone(), maximum))
        }
        serde_json::Value::Array(values) => {
            let mut bounded = Vec::new();
            for value in values {
                let candidate = compact_json(value, maximum.saturating_sub(2));
                bounded.push(candidate);
                if json_size(&serde_json::Value::Array(bounded.clone())) > maximum {
                    bounded.pop();
                    break;
                }
            }
            serde_json::Value::Array(bounded)
        }
        serde_json::Value::Object(object) => {
            let mut bounded = serde_json::Map::new();
            for (key, value) in object {
                let candidate = compact_json(value, maximum.saturating_sub(key.len() + 8));
                bounded.insert(key.clone(), candidate);
                if json_size(&serde_json::Value::Object(bounded.clone())) > maximum {
                    bounded.remove(key);
                    break;
                }
            }
            serde_json::Value::Object(bounded)
        }
        _ => value.clone(),
    }
}

fn json_size(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value)
        .expect("structured output values must be serializable")
        .len()
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn repeated_dispatch_start_preserves_partial_and_completed_outcomes() {
        let progress = ActionProgress::default();
        assert_eq!(progress.snapshot().outcome(), ToolOutcome::NotStarted);

        // GrabFocus and EIS share this record. Re-entering dispatch must not
        // erase the fact that focus already changed if typing is interrupted.
        progress.mark_started();
        progress.mark_cleanup_completed();
        progress.mark_started();
        assert_eq!(progress.snapshot().outcome(), ToolOutcome::Unknown);
        assert_eq!(progress.snapshot().cleanup, CleanupStatus::Completed);

        progress.mark_completed();
        progress.mark_started();
        assert_eq!(progress.snapshot().outcome(), ToolOutcome::Completed);
        assert_eq!(progress.snapshot().dispatch_stage, DispatchStage::Completed);
    }
}
