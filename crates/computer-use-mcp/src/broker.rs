//! One public connection, two independently scheduled, lazily owned desktops.
//! The private pipe protocol returns cancellation completion explicitly: MCP
//! notifications/cancelled deliberately suppresses the ordinary response and
//! therefore cannot serve as a cleanup acknowledgement between these processes.
use std::{
    collections::BTreeMap,
    future::Future,
    io::{self, Read},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerInfo, Tool,
    },
    service::{RequestContext, RoleServer, ServerInitializeError},
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

use crate::{
    contract::{TOOL_NAMES, tool_definitions},
    errors::{CliError, RuntimeError, ToolOutcome},
    runtime::{DesktopRuntime, tool_error_result},
    server::{ComputerUseMcpServer, for_protocol, supports_structured_content},
    validation::{Desktop, RoutedCall, validate_call, validate_routed_call},
};

// The crate-local symlink is materialized by `cargo package`, so installed
// binaries embed the same runner without depending on a checkout at runtime.
const RUNNER: &str = include_str!("../run-isolated-session.sh");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_PIPE_FRAME: usize = 64 * 1024 * 1024;
const RETIRED_RECOVERY: &str = "Session retired; its IDs are invalid. Call list_desktop on the intended desktop, then observe a fresh target. Inspect state before repeating any action whose outcome is unknown or completed.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    desktop: Desktop,
    generation: u64,
    local: String,
}

#[derive(Debug)]
struct Identities {
    next: u64,
    public: BTreeMap<String, Binding>,
    local: BTreeMap<(Desktop, u64, String), String>,
}

impl Identities {
    fn allocate(&mut self) -> u64 {
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .expect("desktop identity space exhausted");
        id
    }

    fn publish(&mut self, desktop: Desktop, generation: u64, local: &str, prefix: &str) -> String {
        let key = (desktop, generation, local.to_owned());
        if let Some(public) = self.local.get(&key) {
            return public.clone();
        }
        let public = format!("{prefix}-{:016x}", self.allocate());
        self.public.insert(
            public.clone(),
            Binding {
                desktop,
                generation,
                local: local.into(),
            },
        );
        self.local.insert(key, public.clone());
        public
    }

    fn decode(
        &self,
        arguments: &mut Value,
        selected: Option<Desktop>,
    ) -> Result<(Desktop, Option<u64>), RuntimeError> {
        let mut route = None;
        visit_ids(arguments, &mut |value, _| {
            let binding = self.public.get(value).ok_or_else(stale_identity)?;
            let identity_route = (binding.desktop, binding.generation);
            if selected.is_some_and(|desktop| desktop != binding.desktop)
                || route.is_some_and(|previous| previous != identity_route)
            {
                return Err(RuntimeError::invalid_arguments(
                    "IDs belong to different desktop sessions",
                ));
            }
            route = Some(identity_route);
            *value = binding.local.clone();
            Ok(())
        })?;
        Ok(route.map_or(
            (selected.unwrap_or_default(), None),
            |(desktop, generation)| (desktop, Some(generation)),
        ))
    }

    fn encode(&mut self, result: &mut CallToolResult, desktop: Desktop, generation: u64) {
        let mut replacements = BTreeMap::new();
        if let Some(structured) = &mut result.structured_content {
            visit_ids(structured, &mut |value, prefix| {
                let public = self.publish(desktop, generation, value, prefix);
                replacements.insert(value.clone(), public.clone());
                *value = public;
                Ok(())
            })
            .expect("output identity translation cannot fail");
        }
        // Text is a projection of the structured result. Replace complete ID
        // tokens only, never substrings (e.g. an ID followed by another digit).
        for content in &mut result.content {
            if let ContentBlock::Text(text) = content {
                text.text = replace_tokens(&text.text, &replacements);
            }
        }
        let mut header = format!(
            "Desktop: {} session=session-{generation:016x}\n",
            desktop.as_str()
        );
        let structured = result.structured_content.get_or_insert_with(|| json!({}));
        let object = structured
            .as_object_mut()
            .expect("desktop results have object structured content");
        let retired = object.remove("session_replacement_required") == Some(json!(true));
        let mut metadata =
            json!({"desktop":desktop.as_str(),"session_id":format!("session-{generation:016x}")})
                .as_object()
                .expect("metadata object")
                .clone();
        if retired {
            if let Some(Value::String(old_recovery)) = object.remove("recovery") {
                for content in &mut result.content {
                    if let ContentBlock::Text(text) = content {
                        text.text = text.text.replace(&old_recovery, RETIRED_RECOVERY);
                    }
                }
            }
            header.push_str(RETIRED_RECOVERY);
            header.push('\n');
            metadata.insert("session_retired".into(), json!(true));
            metadata.insert("recovery".into(), json!(RETIRED_RECOVERY));
        }
        crate::runtime::annotate_mcp_result(result, &header, metadata);
    }
}

fn identity_prefix(key: &str) -> Option<&'static str> {
    match key {
        "app_instance_id" => Some("app"),
        "window_instance_id" => Some("win"),
        "observation_id" | "after_observation_id" | "replacement_observation_id" => Some("obs"),
        "frame_id" | "after_frame_id" | "replacement_frame_id" => Some("frame"),
        "element_id" | "replacement_element_id" => Some("e"),
        "cursor" | "next_cursor" => Some("cur"),
        _ => None,
    }
}

fn visit_ids(
    value: &mut Value,
    visit: &mut impl FnMut(&mut String, &str) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if let (Some(prefix), Value::String(id)) = (identity_prefix(key), &mut *value) {
                    visit(id, prefix)?;
                } else {
                    visit_ids(value, visit)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                visit_ids(value, visit)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn replace_tokens(text: &str, replacements: &BTreeMap<String, String>) -> String {
    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    for (offset, character) in text.char_indices() {
        if !character.is_ascii_alphanumeric() && character != '-' && character != '_' {
            let token = &text[start..offset];
            output.push_str(replacements.get(token).map_or(token, String::as_str));
            output.push(character);
            start = offset + character.len_utf8();
        }
    }
    let token = &text[start..];
    output.push_str(replacements.get(token).map_or(token, String::as_str));
    output
}

fn stale_identity() -> RuntimeError {
    RuntimeError::new(
        "stale_session",
        "identity is not valid in this desktop session",
        ToolOutcome::NotStarted,
        true,
        "Call list_desktop with the intended desktop, then copy fresh target and observation IDs.",
    )
}

#[derive(Debug)]
struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: PipeReader<BufReader<ChildStdout>>,
    generation: u64,
    abandoned: bool,
}

impl Worker {
    fn spawn(desktop: Desktop, generation: u64) -> io::Result<Self> {
        Self::from_command(Self::command(desktop)?, generation)
    }

    fn command(desktop: Desktop) -> io::Result<Command> {
        let executable = std::env::current_exe()?;
        let command = if desktop == Desktop::Background {
            let mut command = Command::new("bash");
            command
                .args(["-c", RUNNER, "computer-use-mcp-background", "--"])
                .arg(&executable)
                .arg("__background_worker");
            // A user's standalone runner override must not change the worker
            // executable or protocol of the installed broker.
            command.env_remove("COMPUTER_USE_MCP_BIN");
            command.env("COMPUTER_USE_MCP_PRIVATE_PORTAL_AUTH", "require");
            command
        } else {
            let mut command = Command::new(executable);
            command.arg("__desktop_worker");
            command
        };
        Ok(command)
    }

    fn from_command(mut command: Command, generation: u64) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0);
        let mut child = command.spawn()?;
        Ok(Self {
            input: Some(child.stdin.take().expect("piped worker stdin")),
            output: PipeReader::new(BufReader::new(
                child.stdout.take().expect("piped worker stdout"),
            )),
            child,
            generation,
            abandoned: false,
        })
    }

    async fn send(&mut self, value: &Value) -> io::Result<()> {
        write_frame(self.input.as_mut().expect("live worker stdin"), value).await
    }

    fn signal(&self, signal: rustix::process::Signal) {
        if let Some(pid) = self
            .child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
        {
            let _ = rustix::process::kill_process_group(pid, signal);
        }
    }

    async fn stop(&mut self) -> io::Result<()> {
        // EOF asks the worker to cancel, finish cleanup, and shut its runtime
        // down. The runner then tears down all owned service groups.
        self.input.take();
        if let Ok(status) = tokio::time::timeout(CLEANUP_TIMEOUT, self.child.wait()).await {
            return status.map(|_| ());
        }
        self.signal(rustix::process::Signal::TERM);
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            self.signal(rustix::process::Signal::KILL);
            self.child.wait().await?;
        }
        Err(io::Error::other(
            "desktop worker shutdown exceeded cleanup deadline",
        ))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Covers transport-task abortion as well as normal shutdown. TERM lets
        // the runner's EXIT trap reap its separately owned service groups.
        self.signal(rustix::process::Signal::TERM);
    }
}

/// An aborted parent task cannot leave an unobserved request running and let
/// its response be mistaken for the next call's response.
struct PendingCall<'a> {
    worker: &'a mut Worker,
    completed: bool,
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.worker.abandoned = true;
            self.worker.signal(rustix::process::Signal::TERM);
        }
    }
}

#[derive(Debug)]
pub struct DesktopBroker {
    foreground: tokio::sync::Mutex<Option<Worker>>,
    background: tokio::sync::Mutex<Option<Worker>>,
    identities: Mutex<Identities>,
    stopping: tokio::sync::watch::Sender<bool>,
    #[cfg(test)]
    spawn: Option<fn(Desktop, u64) -> io::Result<Worker>>,
}

impl DesktopBroker {
    pub fn new() -> Result<Self, CliError> {
        let mut seed = [0; 8];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut seed))
            .map_err(|error| CliError::Mcp(format!("cannot seed desktop identities: {error}")))?;
        Ok(Self {
            foreground: tokio::sync::Mutex::new(None),
            background: tokio::sync::Mutex::new(None),
            identities: Mutex::new(Identities {
                next: u64::from_ne_bytes(seed) >> 1,
                public: BTreeMap::new(),
                local: BTreeMap::new(),
            }),
            stopping: tokio::sync::watch::channel(false).0,
            #[cfg(test)]
            spawn: None,
        })
    }

    pub async fn call(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        cancelled: impl Future<Output = ()> + Send,
    ) -> CallToolResult {
        let mut stopping = self.stopping.subscribe();
        let cancelled = async {
            if *stopping.borrow() {
                return;
            }
            tokio::select! {
                () = cancelled => {},
                _ = stopping.changed() => {},
            }
        };
        match self.call_inner(name, arguments, cancelled).await {
            Ok(result) => result,
            Err(error) => tool_error_result(&error),
        }
    }

    async fn call_inner(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        cancelled: impl Future<Output = ()> + Send,
    ) -> Result<CallToolResult, RuntimeError> {
        let RoutedCall {
            desktop: selected,
            arguments,
            call,
        } = validate_routed_call(name, arguments)?;
        let mutation = call.tracks_action();
        let mut arguments = Value::Object(arguments);
        let (desktop, expected_generation) = self
            .identities
            .lock()
            .expect("identity lock poisoned")
            .decode(&mut arguments, selected)?;
        tokio::pin!(cancelled);
        let slot = match desktop {
            Desktop::Foreground => &self.foreground,
            Desktop::Background => &self.background,
        };
        let mut slot = tokio::select! {
            biased;
            () = &mut cancelled => return Err(cancelled_before_dispatch()),
            slot = slot.lock() => slot,
        };
        if slot.as_ref().is_some_and(|worker| worker.abandoned) {
            stop_slot(&mut slot).await;
        }
        if expected_generation.is_some_and(|generation| {
            slot.as_ref()
                .is_none_or(|worker| worker.generation != generation)
        }) {
            return Err(stale_identity());
        }
        if slot.is_none() {
            let generation = self
                .identities
                .lock()
                .expect("identity lock poisoned")
                .allocate();
            #[cfg(test)]
            let spawn = self.spawn.unwrap_or(Worker::spawn);
            #[cfg(not(test))]
            let spawn = Worker::spawn;
            let worker =
                spawn(desktop, generation).map_err(|error| worker_failure(error, false))?;
            *slot = Some(worker);
            let startup = {
                let mut pending = PendingCall {
                    worker: slot.as_mut().expect("worker just installed"),
                    completed: false,
                };
                let result = tokio::select! {
                    biased;
                    () = &mut cancelled => Err(cancelled_before_dispatch()),
                    result = tokio::time::timeout(STARTUP_TIMEOUT, pending.worker.output.read()) => match result {
                        Ok(Ok(value)) if value == json!({"ready":1}) => Ok(()),
                        Ok(Ok(_)) => Err(worker_failure("invalid worker handshake", false)),
                        Ok(Err(error)) => Err(worker_failure(error, false)),
                        Err(_) => Err(worker_failure("desktop startup timed out", false)),
                    },
                };
                pending.completed = result.is_ok();
                result
            };
            if let Err(error) = startup {
                stop_slot(&mut slot).await;
                return Err(error);
            }
        }
        let generation = slot.as_ref().expect("initialized worker").generation;
        // Cancellation before polling send guarantees no mutation. Once send
        // has been polled, partial pipe writes make dispatch uncertain.
        let result = {
            let mut pending = PendingCall {
                worker: slot.as_mut().expect("initialized worker"),
                completed: false,
            };
            let worker = &mut *pending.worker;
            let result = async {
            tokio::select! {
                biased;
                () = &mut cancelled => return Ok(tool_error_result(&cancelled_before_dispatch())),
                () = std::future::ready(()) => {},
            }
            tokio::time::timeout(CLEANUP_TIMEOUT, worker.send(&json!({"name":name,"arguments":arguments}))).await
                .map_err(|_| io::Error::other("worker request write timed out"))??;
            let response = tokio::select! {
                biased;
                result = worker.output.read() => result?,
                () = &mut cancelled => {
                    worker.send(&json!({"cancel":true})).await?;
                    tokio::time::timeout(CLEANUP_TIMEOUT, worker.output.read()).await
                        .map_err(|_| io::Error::other("worker cancellation cleanup timed out"))??
                }
            };
            serde_json::from_value::<CallToolResult>(response)
                .map_err(|_| io::Error::other("invalid desktop worker response"))
        }.await;
            pending.completed = result.is_ok();
            result
        };
        match result {
            Ok(mut result) => {
                let retire = result
                    .structured_content
                    .as_ref()
                    .is_some_and(|value| value["session_replacement_required"] == true);
                if retire {
                    stop_slot(&mut slot).await;
                }
                self.identities
                    .lock()
                    .expect("identity lock poisoned")
                    .encode(&mut result, desktop, generation);
                Ok(result)
            }
            Err(error) => {
                stop_slot(&mut slot).await;
                Err(worker_failure(error, mutation))
            }
        }
    }

    pub async fn shutdown(&self) {
        self.stopping.send_replace(true);
        let (mut foreground, mut background) =
            tokio::join!(self.foreground.lock(), self.background.lock());
        tokio::join!(stop_slot(&mut foreground), stop_slot(&mut background));
    }
}

async fn stop_slot(slot: &mut Option<Worker>) {
    if let Some(mut worker) = slot.take()
        && let Err(error) = worker.stop().await
    {
        eprintln!("computer-use-mcp: {error}");
    }
}

fn cancelled_before_dispatch() -> RuntimeError {
    RuntimeError::new(
        "cancelled",
        "call cancelled before worker dispatch",
        ToolOutcome::NotStarted,
        true,
        "Retry if still needed.",
    )
}

fn worker_failure(error: impl std::fmt::Display, mutation: bool) -> RuntimeError {
    let mut error = RuntimeError::new(
        "desktop_worker_failed",
        format!("desktop worker unavailable: {error}"),
        if mutation {
            ToolOutcome::Unknown
        } else {
            ToolOutcome::NotStarted
        },
        !mutation,
        "This session's IDs are invalid. Call list_desktop on the intended desktop for a new session. If dispatch may have happened, inspect application state before repeating an action; held-input cleanup is not confirmed.",
    );
    if mutation {
        error.action_progress = Some(
            json!({"dispatch_stage":"started","cleanup":"failed","post_visual":"session_unavailable","post_accessibility":"unavailable"}),
        );
    }
    error
}

impl ServerHandler for DesktopBroker {
    fn get_info(&self) -> ServerInfo {
        crate::server::server_info()
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
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
        if !TOOL_NAMES.contains(&request.name.as_ref()) {
            return Err(McpError::invalid_params("unknown tool", None));
        }
        let result = self
            .call(
                &request.name,
                request.arguments.unwrap_or_default(),
                context.ct.cancelled(),
            )
            .await;
        Ok(for_protocol(result, supports_structured_content(&context)))
    }
}

pub async fn serve_stdio() -> Result<(), CliError> {
    let broker = Arc::new(DesktopBroker::new()?);
    let result = match Arc::clone(&broker).serve(rmcp::transport::stdio()).await {
        Ok(service) => service
            .waiting()
            .await
            .map(|_| ())
            .map_err(|error| CliError::Mcp(format!("MCP server stopped: {error}"))),
        Err(ServerInitializeError::ConnectionClosed(_)) => Ok(()),
        Err(error) => Err(CliError::Mcp(format!("MCP initialization failed: {error}"))),
    };
    broker.shutdown().await;
    result
}

async fn write_frame(output: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await
}

#[derive(Debug)]
struct PipeReader<R> {
    input: R,
    pending: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> PipeReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            pending: Vec::new(),
        }
    }

    // fill_buf cancellation consumes nothing. Bytes already consumed live in
    // self.pending, so a concurrent cancellation cannot split a JSON frame.
    async fn read(&mut self) -> io::Result<Value> {
        loop {
            let available = self.input.fill_buf().await?;
            if available.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "desktop worker pipe closed",
                ));
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let take = end.map_or(available.len(), |end| end + 1);
            if self.pending.len() + take > MAX_PIPE_FRAME {
                return Err(io::Error::other("desktop pipe frame exceeds limit"));
            }
            self.pending.extend_from_slice(&available[..take]);
            self.input.consume(take);
            if end.is_some() {
                return serde_json::from_slice(&std::mem::take(&mut self.pending))
                    .map_err(io::Error::other);
            }
        }
    }
}

pub(crate) async fn worker_stdio<R: DesktopRuntime>(runtime: Arc<R>) -> Result<(), CliError> {
    worker_loop(
        runtime,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
    .map_err(|error| CliError::Mcp(format!("desktop worker protocol failed: {error}")))
}

async fn worker_loop<R: DesktopRuntime>(
    runtime: Arc<R>,
    input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut input = PipeReader::new(input);
    let server = ComputerUseMcpServer::new(Arc::clone(&runtime));
    write_frame(&mut output, &json!({"ready":1})).await?;
    loop {
        let request = match input.read().await {
            Ok(request) => request,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        // A cancellation can race a completed response. It must never cancel
        // the next request, which is sent only after this response is read.
        if request == json!({"cancel":true}) {
            continue;
        }
        let call = request
            .get("name")
            .and_then(Value::as_str)
            .zip(request.get("arguments").and_then(Value::as_object))
            .ok_or_else(|| io::Error::other("invalid worker request"))?;
        let call = match validate_call(call.0, call.1.clone()) {
            Ok(call) => call,
            Err(error) => {
                write_frame(
                    &mut output,
                    &serde_json::to_value(tool_error_result(&error))?,
                )
                .await?;
                continue;
            }
        };
        let (cancel, mut cancellation) = tokio::sync::watch::channel(false);
        let execute = server.execute(call, async move {
            let _ = cancellation.changed().await;
        });
        tokio::pin!(execute);
        let mut eof = false;
        let mut result = tokio::select! {
            result = &mut execute => result,
            message = input.read() => {
                cancel.send_replace(true);
                match message {
                    Ok(value) if value == json!({"cancel":true}) => {},
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => eof = true,
                    _ => { let _ = (&mut execute).await; return Err(io::Error::other("unexpected command while worker busy")); },
                }
                (&mut execute).await
            }
        };
        if eof {
            return Ok(());
        }
        if runtime.desktop_session_exhausted() {
            result.structured_content.get_or_insert_with(|| json!({}))["session_replacement_required"] =
                json!(true);
        }
        let retire = result
            .structured_content
            .as_ref()
            .is_some_and(|value| value["session_replacement_required"] == true);
        write_frame(&mut output, &serde_json::to_value(result)?).await?;
        if retire {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ActionProgress, ToolOutput};
    use crate::validation::ToolCall;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // A real subprocess with deliberately colliding local IDs. It echoes
    // received arguments rather than implementing any broker routing logic.
    fn fixture_worker(desktop: Desktop, generation: u64) -> io::Result<Worker> {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", r#"
import json, os, sys
print(json.dumps({'ready': 1}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    if request.get('cancel'):
        continue
    args = request['arguments']
    if request['name'] == 'launch_application':
        if args['desktop_id'] == 'retire.desktop':
            print(json.dumps({'content':[{'type':'text','text':'completed'}], 'isError':True,
                'structuredContent':{'session_replacement_required':True,'outcome':'completed',
                    'recovery':'Disable and re-enable the MCP.'}}), flush=True)
            continue
        if args['desktop_id'] == 'crash.desktop':
            os._exit(17)
        if args['desktop_id'] == 'block.desktop':
            cancel = json.loads(sys.stdin.readline())
            assert cancel == {'cancel': True}
            print(json.dumps({'content': [{'type':'text','text':'cancel complete'}], 'isError':True,
                'structuredContent':{'outcome':'unknown','action_progress':{'dispatch_stage':'started','cleanup':'completed'}}}), flush=True)
            continue
    content = {'target':{'app_instance_id':'app-0000000000000001','window_instance_id':'win-0000000000000001'},
        'observation_id':'obs-0000000000000001','frame_id':'frame-0000000000000001',
        'element_id':'e-0000000000000001','next_cursor':'cur-windows-0000000000000001-0000000000000001',
        'pid':os.getpid(),'received':json.dumps(args),'worker_desktop':sys.argv[1]}
    print(json.dumps({'content':[{'type':'text','text':'app-0000000000000001 win-0000000000000001 obs-0000000000000001'}],
        'structuredContent':content,'isError':False}), flush=True)
"#, desktop.as_str()]);
        Worker::from_command(command, generation)
    }

    fn broker() -> Arc<DesktopBroker> {
        let mut broker = DesktopBroker::new().unwrap();
        broker.spawn = Some(fixture_worker);
        Arc::new(broker)
    }

    async fn call(broker: &DesktopBroker, name: &str, arguments: Value) -> Value {
        let result = broker
            .call(
                name,
                arguments.as_object().unwrap().clone(),
                std::future::pending(),
            )
            .await;
        result.structured_content.unwrap()
    }

    async fn list(broker: &DesktopBroker, desktop: &str) -> Value {
        call(
            broker,
            "list_desktop",
            json!({"scope":"windows","desktop":desktop}),
        )
        .await
    }

    #[tokio::test]
    async fn routing_is_lazy_persistent_and_namespaces_every_reference() {
        let broker = broker();
        assert!(broker.foreground.lock().await.is_none());
        let background = list(&broker, "background").await;
        assert!(
            broker.foreground.lock().await.is_none(),
            "background must not initialize foreground"
        );
        let foreground = list(&broker, "foreground").await;
        assert_ne!(foreground["pid"], background["pid"]);
        for key in [
            "target",
            "observation_id",
            "frame_id",
            "element_id",
            "next_cursor",
            "session_id",
        ] {
            assert_ne!(foreground[key], background[key], "collision in {key}");
        }
        assert_eq!(list(&broker, "background").await["pid"], background["pid"]);
        let observed = call(
            &broker,
            "observe",
            json!({"target":background["target"],"view":"accessibility"}),
        )
        .await;
        assert_eq!(observed["pid"], background["pid"]);
        let received: Value = serde_json::from_str(observed["received"].as_str().unwrap()).unwrap();
        assert_eq!(
            received["target"]["window_instance_id"],
            "win-0000000000000001"
        );
        let cursor = call(
            &broker,
            "list_desktop",
            json!({"scope":"windows","cursor":background["next_cursor"]}),
        )
        .await;
        assert_eq!(
            cursor["pid"], background["pid"],
            "cursor routes without an explicit selector"
        );
        let mixed = call(&broker, "act", json!({
            "target":background["target"],
            "source_observation":{"observation_id":foreground["observation_id"]},
            "operation":{"type":"semantic","element_id":background["element_id"],"action":{"type":"invoke"}}
        })).await;
        assert_eq!(mixed["code"], "invalid_arguments");
        let text = format!("literal {}", background["element_id"].as_str().unwrap());
        let typed = call(&broker, "act", json!({
            "target":background["target"],
            "source_observation":{"observation_id":background["observation_id"]},
            "operation":{"type":"semantic","element_id":background["element_id"],"action":{"type":"set_value","value":text}}
        })).await;
        let received: Value = serde_json::from_str(typed["received"].as_str().unwrap()).unwrap();
        assert_eq!(
            received["operation"]["action"]["value"], text,
            "payload text is never translated"
        );
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn dead_worker_invalidates_ids_without_poisoning_other_desktop() {
        let broker = broker();
        let old = list(&broker, "background").await;
        let foreground = list(&broker, "foreground").await;
        let death = call(
            &broker,
            "launch_application",
            json!({"desktop":"background","desktop_id":"crash.desktop"}),
        )
        .await;
        assert_eq!(death["outcome"], "unknown");
        assert_eq!(death["retryable"], false);
        assert_eq!(death["action_progress"]["cleanup"], "failed");
        assert_eq!(list(&broker, "foreground").await["pid"], foreground["pid"]);
        let fresh = list(&broker, "background").await;
        assert_ne!(old["target"], fresh["target"]);
        let stale = call(
            &broker,
            "observe",
            json!({"target":old["target"],"view":"accessibility"}),
        )
        .await;
        assert_eq!(stale["code"], "stale_session");
        assert_eq!(stale["outcome"], "not_started");
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn per_desktop_barriers_cancellation_and_aborted_calls_are_isolated() {
        let broker = broker();
        list(&broker, "background").await;
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let task_broker = broker.clone();
        let task = tokio::spawn(async move {
            task_broker
                .call(
                    "launch_application",
                    json!({"desktop":"background","desktop_id":"block.desktop"})
                        .as_object()
                        .unwrap()
                        .clone(),
                    async {
                        let _ = cancelled.changed().await;
                    },
                )
                .await
        });
        // The background slot being locked proves the call crossed scheduling.
        while broker.background.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        let foreground = tokio::time::timeout(Duration::from_secs(2), list(&broker, "foreground"))
            .await
            .unwrap();
        let queued = broker
            .call(
                "launch_application",
                json!({"desktop":"background","desktop_id":"unused.desktop"})
                    .as_object()
                    .unwrap()
                    .clone(),
                std::future::ready(()),
            )
            .await;
        assert_eq!(queued.structured_content.unwrap()["outcome"], "not_started");
        cancel.send_replace(true);
        let result = task.await.unwrap().structured_content.unwrap();
        assert_eq!(result["action_progress"]["cleanup"], "completed");
        let prior = list(&broker, "background").await;
        let task_broker = broker.clone();
        let task = tokio::spawn(async move {
            call(
                &task_broker,
                "launch_application",
                json!({"desktop":"background","desktop_id":"block.desktop"}),
            )
            .await
        });
        while broker.background.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        let fresh = list(&broker, "background").await;
        assert_ne!(fresh["session_id"], prior["session_id"]);
        assert_eq!(list(&broker, "foreground").await["pid"], foreground["pid"]);
        broker.shutdown().await;
    }

    #[test]
    fn background_command_requires_private_preauthorization() {
        let command = Worker::command(Desktop::Background).unwrap();
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "COMPUTER_USE_MCP_PRIVATE_PORTAL_AUTH"
                    && value == Some(std::ffi::OsStr::new("require")))
        );
        assert!(
            !Worker::command(Desktop::Foreground)
                .unwrap()
                .as_std()
                .get_envs()
                .any(|(key, _)| key == "COMPUTER_USE_MCP_PRIVATE_PORTAL_AUTH")
        );
    }

    #[tokio::test]
    async fn exhausted_worker_retires_without_replaying_completed_mutation() {
        let broker = broker();
        let old = list(&broker, "background").await;
        let foreground = list(&broker, "foreground").await;
        let result = call(
            &broker,
            "launch_application",
            json!({"desktop":"background","desktop_id":"retire.desktop"}),
        )
        .await;
        assert_eq!(result["outcome"], "completed");
        assert_eq!(result["session_retired"], true);
        assert_eq!(result["recovery"], RETIRED_RECOVERY);
        assert!(
            broker.background.lock().await.is_none(),
            "retirement must not automatically start a replacement"
        );
        let stale = call(
            &broker,
            "observe",
            json!({"target":old["target"],"view":"accessibility"}),
        )
        .await;
        assert_eq!(stale["code"], "stale_session");
        let fresh = list(&broker, "background").await;
        assert_ne!(fresh["session_id"], old["session_id"]);
        assert_ne!(fresh["target"], old["target"]);
        assert_eq!(list(&broker, "foreground").await["pid"], foreground["pid"]);
        broker.shutdown().await;
    }

    struct InitializationRuntime {
        pending: bool,
        waiting: tokio::sync::Notify,
        calls: AtomicUsize,
    }

    impl DesktopRuntime for InitializationRuntime {
        async fn wait_for_desktop_session(&self) {
            if self.pending {
                self.waiting.notify_one();
                std::future::pending::<()>().await;
            }
        }
        fn desktop_session_exhausted(&self) -> bool {
            self.calls.load(Ordering::Acquire) > 0
        }
        async fn execute(
            &self,
            _: ToolCall,
            _: Option<Arc<ActionProgress>>,
        ) -> Result<ToolOutput, RuntimeError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Err(RuntimeError::not_started(
                "backend_failed",
                "approval denied",
            ))
        }
        async fn cleanup(&self, _: Option<Arc<ActionProgress>>) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn initialization_cancellation_and_exhaustion_end_worker_protocol() {
        for pending in [true, false] {
            let runtime = Arc::new(InitializationRuntime {
                pending,
                waiting: tokio::sync::Notify::new(),
                calls: AtomicUsize::new(0),
            });
            let (client, worker) = tokio::io::duplex(4096);
            let (worker_read, worker_write) = tokio::io::split(worker);
            let worker_runtime = runtime.clone();
            let task = tokio::spawn(async move {
                worker_loop(worker_runtime, BufReader::new(worker_read), worker_write).await
            });
            let (read, mut write) = tokio::io::split(client);
            let mut read = PipeReader::new(BufReader::new(read));
            assert_eq!(read.read().await.unwrap(), json!({"ready":1}));
            write_frame(&mut write, &json!({"name":"observe","arguments":{"view":"screenshot","target":{"app_instance_id":"app-0000000000000001","window_instance_id":"win-0000000000000001"}}})).await.unwrap();
            if pending {
                runtime.waiting.notified().await;
                write_frame(&mut write, &json!({"cancel":true}))
                    .await
                    .unwrap();
            }
            let response = read.read().await.unwrap();
            assert_eq!(
                response["structuredContent"]["session_replacement_required"],
                true
            );
            assert_eq!(response["structuredContent"]["outcome"], "not_started");
            assert_eq!(runtime.calls.load(Ordering::Acquire), usize::from(!pending));
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                read.read().await.unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    struct MutationRuntime {
        calls: AtomicUsize,
        started: tokio::sync::Notify,
        cleaned: AtomicBool,
    }

    impl DesktopRuntime for MutationRuntime {
        async fn execute(
            &self,
            _: ToolCall,
            progress: Option<Arc<ActionProgress>>,
        ) -> Result<ToolOutput, RuntimeError> {
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                progress.unwrap().mark_started();
                self.started.notify_one();
                std::future::pending().await
            } else {
                assert!(
                    self.cleaned.load(Ordering::Acquire),
                    "next call crossed unfinished cleanup"
                );
                Ok(ToolOutput::text("after cleanup"))
            }
        }
        async fn cleanup(&self, progress: Option<Arc<ActionProgress>>) -> Result<(), RuntimeError> {
            tokio::task::yield_now().await;
            self.cleaned.store(true, Ordering::Release);
            if let Some(progress) = progress {
                progress.mark_cleanup_completed();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn worker_protocol_acknowledges_actual_runtime_cleanup_before_reuse() {
        let runtime = Arc::new(MutationRuntime {
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            cleaned: AtomicBool::new(false),
        });
        let (client, worker) = tokio::io::duplex(4096);
        let (worker_read, worker_write) = tokio::io::split(worker);
        let worker_runtime = runtime.clone();
        let task = tokio::spawn(async move {
            worker_loop(worker_runtime, BufReader::new(worker_read), worker_write).await
        });
        let (read, mut write) = tokio::io::split(client);
        let mut read = PipeReader::new(BufReader::new(read));
        assert_eq!(read.read().await.unwrap(), json!({"ready":1}));
        write_frame(
            &mut write,
            &json!({"name":"launch_application","arguments":{"desktop_id":"test.desktop"}}),
        )
        .await
        .unwrap();
        runtime.started.notified().await;
        write_frame(&mut write, &json!({"cancel":true}))
            .await
            .unwrap();
        let response = read.read().await.unwrap();
        assert_eq!(response["structuredContent"]["outcome"], "unknown");
        assert_eq!(
            response["structuredContent"]["action_progress"]["cleanup"],
            "completed"
        );
        write_frame(
            &mut write,
            &json!({"name":"list_desktop","arguments":{"scope":"windows"}}),
        )
        .await
        .unwrap();
        assert_eq!(
            read.read().await.unwrap()["content"][0]["text"],
            "after cleanup"
        );
        write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn interrupted_pipe_reads_keep_partial_frames() {
        let (mut write, read) = tokio::io::duplex(32);
        let mut reader = PipeReader::new(BufReader::new(read));
        write.write_all(b"{\"ready\":").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), reader.read())
                .await
                .is_err()
        );
        write.write_all(b"1}\n").await.unwrap();
        assert_eq!(reader.read().await.unwrap(), json!({"ready":1}));
    }

    #[test]
    fn routing_metadata_respects_result_budgets_and_preserves_raw_structured_text() {
        let broker = broker();
        let mut result = ToolOutput::text(format!("app-0000000000000001 {}", "x".repeat(16_000)))
            .with_structured_content(json!({"target":{"app_instance_id":"app-0000000000000001","window_instance_id":"win-0000000000000001"},"value":"app-0000000000000001","payload":"x".repeat(16_000)}))
            .into_mcp_result();
        broker
            .identities
            .lock()
            .unwrap()
            .encode(&mut result, Desktop::Background, 1);
        assert!(
            result.content[0].as_text().unwrap().text.len() <= crate::runtime::MAX_MODEL_TEXT_BYTES
        );
        let structured = result.structured_content.unwrap();
        assert!(
            serde_json::to_vec(&structured).unwrap().len()
                <= crate::runtime::MAX_MODEL_STRUCTURED_BYTES
        );
        assert_eq!(structured["desktop"], "background");
        assert_eq!(structured["session_id"], "session-0000000000000001");
        assert_eq!(structured["value"], "app-0000000000000001");
        assert_ne!(structured["target"]["app_instance_id"], structured["value"]);
    }

    #[test]
    fn retirement_budget_reserves_metadata_and_preserves_error_outcome() {
        let broker = broker();
        let error = RuntimeError::new(
            "backend_failed",
            "message ".repeat(3_000),
            ToolOutcome::Unknown,
            false,
            "Disable and re-enable the MCP.",
        );
        let mut result = tool_error_result(&error);
        result.structured_content.as_mut().unwrap()["session_replacement_required"] = json!(true);
        broker
            .identities
            .lock()
            .unwrap()
            .encode(&mut result, Desktop::Background, 1);
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.len() <= crate::runtime::MAX_MODEL_TEXT_BYTES);
        assert!(text.starts_with("Desktop: background"));
        assert!(text.contains("Outcome: unknown"));
        assert!(!text.contains("re-enable"));
        let structured = result.structured_content.unwrap();
        assert!(
            serde_json::to_vec(&structured).unwrap().len()
                <= crate::runtime::MAX_MODEL_STRUCTURED_BYTES
        );
        assert_eq!(structured["session_retired"], true);
        assert_eq!(structured["outcome"], "unknown");
        assert_eq!(structured["recovery"], RETIRED_RECOVERY);
    }
}
