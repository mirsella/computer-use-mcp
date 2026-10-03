//! Bounded, content-free diagnostics at the public broker boundary.
use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rmcp::model::{CallToolResult, ContentBlock};
use rustix::fs::{FlockOperation, Mode, OFlags};
use serde_json::{Value, json};

use crate::{
    contract::TOOL_NAMES,
    portal::{open_private_directory, xdg_state_directory},
    validation::{
        ActOperation, Desktop, DesktopScope, ElementAction, KeyboardEvent, PointerAction, ToolCall,
        WaitCondition,
    },
};

const CURRENT: &str = "calls.jsonl";
const PREVIOUS: &str = "calls.previous.jsonl";
const FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct CallHistory {
    directory: Option<PathBuf>,
    connection: String,
    next: AtomicU64,
    warned: AtomicBool,
}

impl CallHistory {
    pub(crate) fn new(seed: u64) -> Self {
        // Unit-test brokers must not populate the user's diagnostic history.
        let directory = if cfg!(test) {
            None
        } else {
            match directory_from_env() {
                Ok(directory) => Some(directory),
                Err(error) => {
                    eprintln!("computer-use-mcp: call history unavailable: {error}");
                    None
                }
            }
        };
        Self {
            directory,
            connection: format!("connection-{seed:016x}"),
            next: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        }
    }

    pub(crate) fn begin(&self, tool: &str) -> CallRecord<'_> {
        let sequence = self.next.fetch_add(1, Ordering::Relaxed);
        let tool = if TOOL_NAMES.contains(&tool) || matches!(tool, "help" | "dispatch") {
            tool
        } else {
            "unknown"
        };
        let data = json!({
            "event":"started",
            "call_id":format!("{}-{sequence}", self.connection),
            "connection_id":self.connection,
            "pid":std::process::id(),
            "started_at_ms":SystemTime::now().duration_since(UNIX_EPOCH).expect("clock predates Unix epoch").as_millis(),
            "tool":tool,
        });
        CallRecord {
            history: self,
            data,
            started: Instant::now(),
            logged: false,
            finished: false,
        }
    }

    fn append(&self, value: &Value) {
        let Some(path) = &self.directory else { return };
        let result = (|| -> Result<(), String> {
            let mut bytes = serde_json::to_vec(value).expect("history record serializes");
            bytes.push(b'\n');
            if bytes.len() as u64 > FILE_BYTES {
                return Err("call history record exceeds its file budget".into());
            }
            let directory = open_private_directory(path, true)?.expect("created history directory");
            let _lock = lock_directory(&directory).map_err(|error| error.to_string())?;
            let mut file =
                open_file(&directory, CURRENT, true).map_err(|error| error.to_string())?;
            repair_tail(&mut file).map_err(|error| error.to_string())?;
            if file.metadata().map_err(|error| error.to_string())?.len() + bytes.len() as u64
                > FILE_BYTES
            {
                rustix::fs::renameat(&directory, CURRENT, &directory, PREVIOUS)
                    .map_err(|error| error.to_string())?;
                file = open_file(&directory, CURRENT, true).map_err(|error| error.to_string())?;
            }
            file.seek(SeekFrom::End(0))
                .and_then(|_| file.write_all(&bytes))
                .map_err(|error| error.to_string())
        })();
        if let Err(error) = result {
            if !self.warned.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "computer-use-mcp: cannot write call history at {}: {error}",
                    path.display()
                );
            }
        } else {
            self.warned.store(false, Ordering::Relaxed);
        }
    }
}

pub(crate) struct CallRecord<'a> {
    history: &'a CallHistory,
    data: Value,
    started: Instant,
    logged: bool,
    finished: bool,
}

impl CallRecord<'_> {
    pub(crate) fn action(&mut self, action: &str) {
        if TOOL_NAMES.contains(&action) {
            self.data["action"] = json!(action);
        }
    }

    pub(crate) fn request(&mut self, action: &str, call: &ToolCall) {
        self.action(action);
        self.data["request"] = request_summary(call);
    }

    pub(crate) fn route(&mut self, desktop: Desktop) {
        self.data["desktop"] = json!(desktop.as_str());
        self.start();
    }

    fn start(&mut self) {
        if !self.logged {
            self.logged = true;
            self.history.append(&self.data);
        }
    }

    pub(crate) fn finish(&mut self, result: &CallToolResult) {
        self.start();
        self.data["status"] = json!(if result.is_error == Some(true) {
            "error"
        } else {
            "succeeded"
        });
        self.data["result"] = result_summary(result);
        self.complete();
    }

    pub(crate) fn protocol_error(&mut self) {
        self.start();
        self.data["status"] = json!("error");
        self.data["result"] =
            json!({"code":"invalid_params","outcome":"not_started","protocol_error":true});
        self.complete();
    }

    fn complete(&mut self) {
        self.finished = true;
        self.data["event"] = json!("finished");
        self.data["duration_ms"] = json!(self.started.elapsed().as_millis());
        self.history.append(&self.data);
    }
}

impl Drop for CallRecord<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.start();
            self.data["status"] = json!("abandoned");
            // A dropped future supplies no authoritative dispatch outcome.
            self.data["result"] = json!({"outcome":"unknown"});
            self.complete();
        }
    }
}

pub(crate) fn directory_from_env() -> Result<PathBuf, String> {
    xdg_state_directory().map(|directory| directory.join("history"))
}

#[derive(Default)]
pub(crate) struct Query {
    errors_only: bool,
    last: Option<usize>,
    since_ms: Option<u128>,
    call_id: Option<String>,
}

impl Query {
    pub(crate) fn parse(arguments: &[String], now: SystemTime) -> Result<Self, String> {
        let mut query = Self::default();
        let mut arguments = arguments.iter();
        while let Some(option) = arguments.next() {
            match option.as_str() {
                "--errors" if !query.errors_only => query.errors_only = true,
                "--last" if query.last.is_none() => {
                    query.last = Some(
                        arguments
                            .next()
                            .and_then(|value| value.parse().ok())
                            .ok_or("history --last requires a nonnegative integer")?,
                    );
                }
                "--since" if query.since_ms.is_none() => {
                    let value = arguments.next().ok_or("history --since requires a time")?;
                    query.since_ms = Some(match [("s", 1_000), ("m", 60_000), ("h", 3_600_000), ("d", 86_400_000)]
                        .into_iter().find_map(|(suffix, multiplier)| value.strip_suffix(suffix).map(|number| (number, multiplier))) {
                        Some((number, multiplier)) => {
                            let duration = number.parse::<u128>().ok()
                                .and_then(|number| number.checked_mul(multiplier))
                                .ok_or("history --since requires an integer duration such as 15m or 2h")?;
                            now.duration_since(UNIX_EPOCH).map_err(|_| "clock predates Unix epoch")?
                                .as_millis().saturating_sub(duration)
                        }
                        None => value.parse().map_err(|_| "history --since requires Unix milliseconds or an integer duration ending in s, m, h, or d")?,
                    });
                }
                "--call-id" if query.call_id.is_none() => {
                    query.call_id = Some(
                        arguments
                            .next()
                            .filter(|value| !value.is_empty() && !value.starts_with("--"))
                            .ok_or("history --call-id requires a call ID")?
                            .clone(),
                    );
                }
                _ => return Err("unknown or duplicate history option".into()),
            }
        }
        Ok(query)
    }

    fn filter(&self, mut records: Vec<Value>) -> Vec<Value> {
        records.retain(|record| {
            (!self.errors_only || record["status"] == "error" || record["status"] == "abandoned")
                && self
                    .call_id
                    .as_ref()
                    .is_none_or(|id| record["call_id"] == *id)
                && self.since_ms.is_none_or(|since| {
                    record["started_at_ms"]
                        .as_u64()
                        .is_some_and(|time| u128::from(time) >= since)
                })
        });
        if let Some(last) = self.last {
            let mut calls = HashSet::new();
            for record in records.iter().rev() {
                if calls.len() == last {
                    break;
                }
                if let Some(id) = record["call_id"].as_str() {
                    calls.insert(id.to_owned());
                }
            }
            records.retain(|record| {
                record["call_id"]
                    .as_str()
                    .is_some_and(|id| calls.contains(id))
            });
        }
        records
    }
}

pub(crate) fn read(query: &Query) -> Result<Vec<Value>, String> {
    read_at(&directory_from_env()?, query)
}

fn read_at(path: &std::path::Path, query: &Query) -> Result<Vec<Value>, String> {
    let Some(directory) = open_private_directory(path, false)? else {
        return Ok(Vec::new());
    };
    let _lock = lock_directory(&directory).map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for name in [PREVIOUS, CURRENT] {
        let mut file = match open_file(&directory, name, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let mut bytes = Vec::new();
        (&mut file)
            .take(FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > FILE_BYTES {
            return Err("call history file exceeds its budget".into());
        }
        // A killed writer may leave an incomplete final record.
        for line in bytes
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| line.ends_with(b"\n"))
        {
            let value: Value = serde_json::from_slice(line)
                .map_err(|error| format!("invalid call history: {error}"))?;
            records.push(value);
        }
    }
    Ok(query.filter(records))
}

fn lock_directory(directory: &File) -> io::Result<File> {
    let lock = open_file(directory, "calls.lock", true)?;
    rustix::fs::flock(&lock, FlockOperation::LockExclusive)?;
    Ok(lock)
}

fn open_file(directory: &File, name: &str, create: bool) -> io::Result<File> {
    let flags = OFlags::CLOEXEC
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | if create {
            OFlags::RDWR | OFlags::CREATE
        } else {
            OFlags::RDONLY
        };
    let file = File::from(rustix::fs::openat(
        directory,
        name,
        flags,
        Mode::from(0o600),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("call history path is not a regular file"));
    }
    Ok(file)
}

fn repair_tail(file: &mut File) -> io::Result<()> {
    if file.metadata()?.len() == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        (&mut *file).take(FILE_BYTES + 1).read_to_end(&mut bytes)?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        file.set_len(end as u64)?;
    }
    Ok(())
}

fn request_summary(call: &ToolCall) -> Value {
    match call {
        ToolCall::ListDesktop { scope, limit, .. } => {
            json!({"scope":match scope { DesktopScope::Windows => "windows", DesktopScope::Applications => "applications" },"limit":limit})
        }
        ToolCall::LaunchApplication { desktop_id } => json!({"desktop_id":desktop_id}),
        ToolCall::ActivateWindow { target, action } => {
            json!({"target":target.as_json(),"action":action.as_str()})
        }
        ToolCall::Observe {
            target, view, crop, ..
        } => json!({"target":target.as_json(),"view":view.as_str(),"crop":crop.as_str()}),
        ToolCall::Act {
            target,
            source,
            operation,
        } => {
            let operation = match operation {
                ActOperation::Pointer { action } => {
                    json!({"type":"pointer","action":match action { PointerAction::Move { .. } => "move", PointerAction::Click { .. } => "click", PointerAction::Drag { .. } => "drag", PointerAction::Scroll { .. } => "scroll" }})
                }
                ActOperation::Semantic { element_id, action } => {
                    json!({"type":"semantic","element_id":element_id,"action":match action { ElementAction::Invoke => "invoke", ElementAction::Named(_) => "named", ElementAction::Focus => "focus", ElementAction::SetValue(_) => "set_value" },"text_chars":match action { ElementAction::SetValue(value) => Some(value.chars().count()), _ => None }})
                }
                ActOperation::Keyboard { events, .. } => {
                    json!({"type":"keyboard","events":events.len(),"text_chars":events.iter().map(|event| match event { KeyboardEvent::Type(text) => text.chars().count(), KeyboardEvent::Press(_) => 0 }).sum::<usize>()})
                }
                ActOperation::Paste { text, .. } => {
                    json!({"type":"paste","text_chars":text.chars().count()})
                }
            };
            json!({"target":target.as_json(),"observation_id":source.observation_id,"frame_id":source.frame_id,"operation":operation})
        }
        ToolCall::WaitFor {
            target,
            condition,
            timeout_ms,
        } => {
            json!({"target":target.as_ref().map(|target| target.as_json()),"condition":match condition { WaitCondition::HumanIdle => "human_idle", WaitCondition::FrameAdvanced { .. } => "frame_advanced", WaitCondition::FrameChanged { .. } => "frame_changed", WaitCondition::FrameStable { .. } => "frame_stable", WaitCondition::AccessibilityAdvanced { .. } => "accessibility_advanced", WaitCondition::ElementState { .. } => "element_state", WaitCondition::ElementValue { .. } => "element_value", WaitCondition::WindowOpened { .. } => "window_opened", WaitCondition::WindowClosed { .. } => "window_closed" },"timeout_ms":timeout_ms})
        }
    }
}

fn result_summary(result: &CallToolResult) -> Value {
    let mut summary = json!({
        "text_bytes":result.content.iter().filter_map(|content| content.as_text()).map(|text| text.text.len()).sum::<usize>(),
        "image_count":result.content.iter().filter(|content| matches!(content, ContentBlock::Image(_))).count(),
    });
    if let Some(value) = &result.structured_content {
        for key in [
            "code",
            "outcome",
            "session_id",
            "observation_id",
            "frame_id",
            "replacement_observation_id",
            "replacement_frame_id",
        ] {
            if let Some(text) = value[key].as_str().filter(|text| {
                text.len() <= 128
                    && text
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            }) {
                summary[key] = json!(text);
            }
        }
        for key in ["retryable", "satisfied", "session_retired"] {
            if let Some(value) = value[key].as_bool() {
                summary[key] = json!(value)
            }
        }
        if let Some(progress) = value["action_progress"].as_object() {
            let mut projected = json!({});
            for key in [
                "dispatch_stage",
                "cleanup",
                "post_visual",
                "post_accessibility",
            ] {
                if let Some(text) = progress.get(key).and_then(Value::as_str).filter(|text| {
                    text.len() <= 64
                        && text
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                }) {
                    projected[key] = json!(text);
                }
            }
            summary["action_progress"] = projected;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{errors::RuntimeError, runtime::tool_error_result, validation::validate_call};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    struct Fixture {
        history: CallHistory,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let seed = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "computer-use-mcp-history-{}-{seed}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            let mut history = CallHistory::new(seed);
            history.directory = Some(path);
            Self { history }
        }

        fn records(&self, errors_only: bool) -> Vec<Value> {
            read_at(
                self.history.directory.as_ref().unwrap(),
                &Query {
                    errors_only,
                    ..Query::default()
                },
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self.history.directory.as_ref().unwrap()).unwrap();
        }
    }

    #[test]
    fn failures_and_abandoned_calls_preserve_progress_without_payloads() {
        let fixture = Fixture::new();
        let secret = "private typed text";
        let call = validate_call("act", json!({
            "target":{"app_instance_id":"app-0000000000000001","window_instance_id":"win-0000000000000001"},
            "source_observation":{"observation_id":"obs-0000000000000001"},
            "operation":{"type":"semantic","element_id":"e-0000000000000001","action":{"type":"set_value","value":secret}}
        }).as_object().unwrap().clone()).unwrap();
        let mut record = fixture.history.begin("dispatch");
        record.request("act", &call);
        record.route(Desktop::Background);
        let mut error = RuntimeError::new(
            "backend_failed",
            secret,
            crate::errors::ToolOutcome::Unknown,
            false,
            secret,
        );
        error.action_progress = Some(
            json!({"dispatch_stage":"started","cleanup":"failed","post_visual":"not_run","post_accessibility":"not_run","unexpected":secret}),
        );
        record.finish(&tool_error_result(&error));
        drop(record);
        drop(fixture.history.begin("observe"));
        let errors = fixture.records(true);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0]["tool"], "dispatch");
        assert_eq!(errors[0]["action"], "act");
        assert_eq!(errors[0]["desktop"], "background");
        assert_eq!(errors[0]["result"]["outcome"], "unknown");
        assert_eq!(errors[0]["result"]["action_progress"]["cleanup"], "failed");
        assert_eq!(errors[1]["status"], "abandoned");
        assert_eq!(errors[1]["result"]["outcome"], "unknown");
        let records = fixture.records(false);
        assert_eq!(
            records.len(),
            4,
            "one start and one terminal record per call"
        );
        assert!(!serde_json::to_string(&records).unwrap().contains(secret));
        assert_eq!(records[0]["call_id"], records[1]["call_id"]);
        assert!(records[1]["duration_ms"].is_number());
    }

    #[test]
    fn rotation_and_restart_keep_recent_records_and_repair_partial_writes() {
        let fixture = Fixture::new();
        let path = fixture.history.directory.as_ref().unwrap();
        let padding = "x".repeat(64 * 1024);
        for number in 0..40 {
            fixture
                .history
                .append(&json!({"number":number,"padding":padding}));
        }
        for name in [CURRENT, PREVIOUS] {
            let metadata = std::fs::metadata(path.join(name)).unwrap();
            assert!(metadata.len() <= FILE_BYTES);
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let records = fixture.records(false);
        assert!(records.len() < 40);
        assert_eq!(records.last().unwrap()["number"], 39);
        let first = records.first().unwrap()["number"].as_u64().unwrap();
        for (offset, record) in records.iter().enumerate() {
            assert_eq!(record["number"], first + offset as u64);
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(path.join(CURRENT))
            .unwrap()
            .write_all(b"{\"partial")
            .unwrap();
        assert_eq!(fixture.records(false), records);
        let mut restarted = CallHistory::new(123);
        restarted.directory = Some(path.clone());
        restarted
            .begin("help")
            .finish(&CallToolResult::success(Vec::new()));
        let after = fixture.records(false);
        assert_eq!(after.len(), records.len() + 2);
        assert_eq!(after.last().unwrap()["status"], "succeeded");
    }

    #[test]
    fn simultaneous_writers_do_not_lose_or_interleave_records() {
        let fixture = Arc::new(Fixture::new());
        let threads: Vec<_> = (0..4)
            .map(|number| {
                let fixture = Arc::clone(&fixture);
                std::thread::spawn(move || {
                    let mut history = CallHistory::new(number);
                    history.directory = fixture.history.directory.clone();
                    for _ in 0..20 {
                        history
                            .begin("help")
                            .finish(&CallToolResult::success(Vec::new()));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap()
        }
        assert_eq!(fixture.records(false).len(), 160);
        assert!(fixture.records(true).is_empty());
    }

    #[test]
    fn query_filters_whole_calls_in_append_order_even_when_their_records_interleave() {
        let records = vec![
            json!({"call_id":"a","event":"started","started_at_ms":1000}),
            json!({"call_id":"b","event":"started","started_at_ms":2000}),
            json!({"call_id":"b","event":"finished","started_at_ms":2000,"status":"error"}),
            json!({"call_id":"c","event":"started","started_at_ms":3000}),
            json!({"call_id":"c","event":"finished","started_at_ms":3000,"status":"succeeded"}),
            json!({"call_id":"a","event":"finished","started_at_ms":1000,"status":"error"}),
        ];
        let parse = |arguments: &[&str]| {
            Query::parse(
                &arguments
                    .iter()
                    .map(|argument| (*argument).into())
                    .collect::<Vec<_>>(),
                UNIX_EPOCH + std::time::Duration::from_millis(63_000),
            )
            .unwrap()
        };
        assert_eq!(
            parse(&["--last", "1"]).filter(records.clone()),
            vec![records[0].clone(), records[5].clone()]
        );
        assert_eq!(
            parse(&["--errors", "--last", "1"]).filter(records.clone()),
            vec![records[5].clone()]
        );
        assert_eq!(
            parse(&["--since", "1m"]).filter(records.clone()),
            records[3..5]
        );
        assert_eq!(
            parse(&["--since", "3000"]).filter(records.clone()),
            records[3..5]
        );
        assert_eq!(
            parse(&["--call-id", "b"]).filter(records.clone()),
            records[1..3]
        );
        assert_eq!(
            parse(&["--call-id", "b", "--errors", "--since", "2m", "--last", "1"])
                .filter(records.clone()),
            vec![records[2].clone()]
        );
        assert!(parse(&["--last", "0"]).filter(records.clone()).is_empty());
        assert!(parse(&["--call-id", "missing"]).filter(records).is_empty());
    }

    #[test]
    fn query_rejects_missing_duplicate_and_invalid_arguments() {
        for arguments in [
            vec!["--last"],
            vec!["--last", "-1"],
            vec!["--last", "1", "--last", "2"],
            vec!["--since"],
            vec!["--since", "1.5h"],
            vec!["--since", "yesterday"],
            vec!["--since", "1m", "--since", "2m"],
            vec!["--call-id"],
            vec!["--call-id", "--errors"],
            vec!["--errors", "--errors"],
            vec!["--unknown"],
        ] {
            let arguments = arguments.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(
                Query::parse(&arguments, UNIX_EPOCH).is_err(),
                "{arguments:?}"
            );
        }
    }
}
