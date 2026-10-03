use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    NotStarted,
    Unknown,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleAuthority {
    Catalog,
    Observation,
}

impl StaleAuthority {
    const fn recovery(self) -> &'static str {
        match self {
            Self::Catalog => {
                "Call list_desktop again, then retry only if the exact target or inventory entry is still available."
            }
            Self::Observation => {
                "Call observe again for current state, then retry only if the requested action is still needed."
            }
        }
    }
}

impl ToolOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Unknown => "unknown",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub code: &'static str,
    pub message: String,
    pub outcome: ToolOutcome,
    pub retryable: bool,
    pub recovery: String,
    pub action_progress: Option<serde_json::Value>,
}

impl RuntimeError {
    pub(crate) const HUMAN_INPUT_BUSY: &'static str = "HumanInputBusy";

    pub(crate) fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(
            "invalid_arguments",
            message,
            ToolOutcome::NotStarted,
            true,
            "Correct the arguments using the tool input schema, then retry.",
        )
    }

    pub(crate) fn unsupported_desktop_focus_switch() -> Self {
        Self::new(
            "unsupported_action",
            "desktop focus-switch shortcut Alt+Tab is not supported",
            ToolOutcome::NotStarted,
            false,
            "No input was dispatched. Do not retry Alt+Tab or alternate spellings. Use an advertised semantic focus target or launch_application; otherwise stop.",
        )
    }

    /// Temporary human-input refusal. Callers attach actual dispatch progress;
    /// an explicit idle wait and a fresh observation permit resumption.
    pub(crate) fn human_input_busy() -> Self {
        Self::new(
            Self::HUMAN_INPUT_BUSY,
            "human input is active or recent; agent dispatch was refused or interrupted; inspect action progress for dispatch and cleanup status",
            ToolOutcome::NotStarted,
            false,
            "Pause foreground actions. Call wait_for with condition {\"type\":\"human_idle\"} and timeout_ms 120000, or hand control to the user. After satisfied=true, observe fresh state before deciding whether another action is needed. Human activity requires 60 seconds of quiet; cooperative handoff signals must be cleared. An interrupted action is never replayed automatically.",
        )
    }

    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        outcome: ToolOutcome,
        retryable: bool,
        recovery: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            outcome,
            retryable,
            recovery: recovery.into(),
            action_progress: None,
        }
    }

    pub fn not_started(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            ToolOutcome::NotStarted,
            true,
            "Call observe for current state, then retry only if the requested action is still needed.",
        )
    }

    pub fn stale(authority: StaleAuthority, message: impl Into<String>) -> Self {
        Self::new(
            "stale_state",
            message,
            ToolOutcome::NotStarted,
            true,
            authority.recovery(),
        )
    }

    pub fn with_execution_status(
        mut self,
        outcome: ToolOutcome,
        retryable: bool,
        recovery: impl Into<String>,
    ) -> Self {
        self.outcome = outcome;
        self.retryable = retryable;
        self.recovery = recovery.into();
        self
    }
}

impl Display for RuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    InvalidCommand(String),
    InvalidArguments(String),
    Mcp(String),
}

impl Display for CliError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommand(command) => write!(formatter, "unknown command: {command}"),
            Self::InvalidArguments(message) | Self::Mcp(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CliError {}
