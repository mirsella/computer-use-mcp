use std::{
    io::{Read, Write},
    time::Duration,
};

use serde_json::{Map as JsonObject, Value};

use crate::{
    VERSION,
    accessibility::SemanticRuntime,
    atspi_adapter::AtspiAdapter,
    errors::CliError,
    portal::{PortalApproval, PortalBackend, XdgPortalBackend, validate_capabilities},
    runtime::{DesktopRuntime, tool_error_result},
    server,
    validation::validate_call,
};

const HELP: &str = "Computer Use MCP for Linux Wayland\n\nUsage:\n  computer-use-mcp [command]\n\nCommands:\n  init          Ask KDE to approve one monitor and save its restore token.\n  mcp           Restore or request KDE approval, then start the stdio MCP server.\n  call FILE     Execute one call object or an array of calls in one stateful runtime; use - for stdin.\n  list-apps     List live apps with accessible top-level windows.\n  snapshot APP  Print a bounded text-only AT-SPI snapshot.\n  doctor        Report Wayland, portal, PipeWire, AT-SPI, and input prerequisites without prompting.\n  help          Show this help.\n  version       Print the CLI version.\n\nCall input uses {\"name\":\"list_applications\",\"arguments\":{\"scope\":\"running\"}} objects and prints one standard MCP result per line. The MCP command requests KDE approval at startup. Run init only to approve it separately before enabling the MCP. KDE may ask again after revocation or display changes.\n";

pub async fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), CliError> {
    let arguments: Vec<_> = arguments.into_iter().collect();
    let command = arguments.first().map(String::as_str).unwrap_or("help");
    match command {
        "help" | "--help" | "-h" => {
            require_no_extra_arguments(&arguments)?;
            print!("{HELP}");
            Ok(())
        }
        "version" | "--version" | "-V" => {
            require_no_extra_arguments(&arguments)?;
            println!("{VERSION}");
            Ok(())
        }
        "doctor" => {
            require_no_extra_arguments(&arguments)?;
            doctor().await;
            Ok(())
        }
        "init" => {
            require_no_extra_arguments(&arguments)?;
            eprintln!(
                "computer-use-mcp: KDE will ask you to approve exactly one monitor plus keyboard and pointer access"
            );
            let PortalApproval {
                session,
                restore_token_saved,
                ..
            } = XdgPortalBackend::persistent()
                .map_err(CliError::Mcp)?
                .approve()
                .await
                .map_err(CliError::Mcp)?;
            tokio::time::timeout(
                Duration::from_secs(2),
                session.close("initial portal approval completed"),
            )
            .await
            .map_err(|_| CliError::Mcp("timed out closing the temporary portal session".into()))?
            .map_err(CliError::Mcp)?;
            if !restore_token_saved {
                return Err(CliError::Mcp(
                    "KDE approved the temporary session, but no reusable restore token was saved"
                        .to_owned(),
                ));
            }
            println!(
                "Portal approval initialized. Future computer-use sessions will ask KDE to restore it."
            );
            Ok(())
        }
        "list-apps" => {
            require_no_extra_arguments(&arguments)?;
            let runtime = SemanticRuntime::new(AtspiAdapter::default());
            println!(
                "{}",
                runtime
                    .list_apps_text()
                    .await
                    .map_err(|error| CliError::Mcp(error.to_string()))?
            );
            Ok(())
        }
        "snapshot" => {
            if arguments.len() != 2 || arguments[1].trim().is_empty() {
                return Err(CliError::InvalidArguments(
                    "snapshot requires exactly one non-empty APP argument".to_owned(),
                ));
            }
            let runtime = SemanticRuntime::new(AtspiAdapter::default());
            println!(
                "{}",
                runtime
                    .snapshot_text(arguments[1].clone(), None, None, None)
                    .await
                    .map_err(|error| CliError::Mcp(error.to_string()))?
            );
            Ok(())
        }
        "call" => {
            if arguments.len() != 2 || arguments[1].trim().is_empty() {
                return Err(CliError::InvalidArguments(
                    "call requires exactly one FILE argument; use - to read JSON from stdin"
                        .to_owned(),
                ));
            }
            run_calls(&arguments[1]).await
        }
        "mcp" => {
            require_no_extra_arguments(&arguments)?;
            server::serve_stdio().await
        }
        unknown => Err(CliError::InvalidCommand(unknown.to_owned())),
    }
}

async fn run_calls(source: &str) -> Result<(), CliError> {
    let input = if source == "-" {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| {
                CliError::InvalidArguments(format!("failed to read stdin: {error}"))
            })?;
        input
    } else {
        std::fs::read_to_string(source).map_err(|error| {
            CliError::InvalidArguments(format!("failed to read call file {source:?}: {error}"))
        })?
    };
    let value: Value = serde_json::from_str(&input).map_err(|error| {
        CliError::InvalidArguments(format!("call input is not valid JSON: {error}"))
    })?;
    let calls = match value {
        Value::Array(calls) => calls,
        call @ Value::Object(_) => vec![call],
        _ => {
            return Err(CliError::InvalidArguments(
                "call input must be an object or an array of objects".to_owned(),
            ));
        }
    };
    if calls.is_empty() {
        return Err(CliError::InvalidArguments(
            "call input must contain at least one call".to_owned(),
        ));
    }

    let runtime = server::production_runtime();
    let result = {
        let stdout = std::io::stdout();
        execute_calls(runtime.as_ref(), calls, &mut stdout.lock()).await
    };
    let shutdown = runtime
        .shutdown()
        .await
        .map_err(|error| CliError::Mcp(format!("direct-call shutdown failed: {error}")));
    match (result, shutdown) {
        (Ok(()), shutdown) => shutdown,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(shutdown)) => {
            eprintln!("computer-use-mcp: {shutdown}");
            Err(error)
        }
    }
}

async fn execute_calls<R: DesktopRuntime, W: Write>(
    runtime: &R,
    calls: Vec<Value>,
    output: &mut W,
) -> Result<(), CliError> {
    for (index, value) in calls.into_iter().enumerate() {
        let number = index + 1;
        let (name, arguments) = parse_call(value, index)?;
        let call = validate_call(&name, arguments).map_err(|error| {
            CliError::InvalidArguments(format!("call {number} ({name}) is invalid: {error}"))
        })?;
        let (result, failed) = match runtime.execute(call).await {
            Ok(output) => (output.into_mcp_result(), false),
            Err(error) => (tool_error_result(&error), true),
        };
        serde_json::to_writer(&mut *output, &result).map_err(|error| {
            CliError::Mcp(format!("failed to write direct-call result: {error}"))
        })?;
        output.write_all(b"\n").map_err(|error| {
            CliError::Mcp(format!("failed to write direct-call result: {error}"))
        })?;
        output.flush().map_err(|error| {
            CliError::Mcp(format!("failed to flush direct-call result: {error}"))
        })?;
        if failed {
            return Err(CliError::Mcp(format!(
                "call {number} ({name}) failed; remaining calls were not executed"
            )));
        }
    }
    Ok(())
}

fn parse_call(value: Value, index: usize) -> Result<(String, JsonObject<String, Value>), CliError> {
    let number = index + 1;
    let Value::Object(mut object) = value else {
        return Err(CliError::InvalidArguments(format!(
            "call {number} must be an object"
        )));
    };
    let name = object
        .remove("name")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CliError::InvalidArguments(format!("call {number} requires a non-empty string name"))
        })?;
    let arguments = match object.remove("arguments") {
        Some(Value::Object(arguments)) => arguments,
        None => JsonObject::new(),
        Some(_) => {
            return Err(CliError::InvalidArguments(format!(
                "call {number} arguments must be an object"
            )));
        }
    };
    if let Some(field) = object.keys().next() {
        return Err(CliError::InvalidArguments(format!(
            "call {number} has unknown field {field:?}"
        )));
    }
    Ok((name, arguments))
}

async fn doctor() {
    println!("Computer Use MCP doctor");
    println!("This check never opens a portal session or prompts for consent.");

    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty());
    let wayland_ready = session_type.as_deref() == Some("wayland") && display.is_some();
    println!("\n[Wayland session]");
    print_doctor_status(wayland_ready);
    println!(
        "Session type: {}",
        session_type.as_deref().unwrap_or("<unset>")
    );
    println!("Display: {}", display.as_deref().unwrap_or("<unset>"));
    if !wayland_ready {
        println!("Action: run this command inside a KDE Plasma Wayland login session.");
    }

    println!("\n[Accessibility (AT-SPI)]");
    let atspi = SemanticRuntime::new(AtspiAdapter::default());
    print_doctor_result(atspi.list_apps_text().await);

    println!("\n[XDG desktop portal]");
    match XdgPortalBackend::default().capabilities().await {
        Ok(capabilities) => {
            let validation = validate_capabilities(&capabilities);
            print_doctor_status(validation.is_ok());
            println!(
                "RemoteDesktop: v{} (need v2+)",
                capabilities.remote_desktop_version
            );
            println!(
                "ScreenCast: v{} (need v3+)",
                capabilities.screencast_version
            );
            println!(
                "Keyboard input: {}",
                availability(capabilities.available_device_types & 1 != 0)
            );
            println!(
                "Pointer input: {}",
                availability(capabilities.available_device_types & 2 != 0)
            );
            println!(
                "Monitor capture: {}",
                availability(capabilities.available_source_types & 1 != 0)
            );
            println!(
                "Cursor capture: {}",
                availability(capabilities.available_cursor_modes & 3 != 0)
            );
            if let Err(error) = validation {
                println!("Detail: {error}");
            }
        }
        Err(error) => {
            println!("Status: UNAVAILABLE");
            println!("Detail: {error}");
        }
    }

    println!("\n[PipeWire]");
    print_doctor_result(check_pipewire());

    println!("\n[Portal approval and EIS input]");
    println!("Status: NOT TESTED");
    println!("Reason: verifying monitor approval and EIS routing would require consent.");
    println!("Action: run `computer-use-mcp init`, then use the MCP server.");
}

fn print_doctor_result<T, E: std::fmt::Display>(result: Result<T, E>) {
    match result {
        Ok(_) => print_doctor_status(true),
        Err(error) => {
            print_doctor_status(false);
            println!("Detail: {error}");
        }
    }
}

fn print_doctor_status(ready: bool) {
    println!("Status: {}", if ready { "READY" } else { "UNAVAILABLE" });
}

fn availability(available: bool) -> &'static str {
    if available { "available" } else { "missing" }
}

fn check_pipewire() -> Result<(), pipewire::Error> {
    pipewire::init();
    let main_loop = pipewire::main_loop::MainLoopRc::new(None)?;
    let context = pipewire::context::ContextRc::new(&main_loop, None)?;
    let _core = context.connect_rc(None)?;
    Ok(())
}

fn require_no_extra_arguments(arguments: &[String]) -> Result<(), CliError> {
    if arguments.len() > 1 {
        return Err(CliError::InvalidArguments(format!(
            "{} does not accept arguments",
            arguments[0]
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::{errors::RuntimeError, runtime::ToolOutput, validation::ToolCall};

    struct FakeRuntime {
        calls: Mutex<Vec<ToolCall>>,
        fail_at: Option<usize>,
    }

    impl DesktopRuntime for FakeRuntime {
        async fn execute(&self, call: ToolCall) -> Result<ToolOutput, RuntimeError> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(call);
            if self.fail_at == Some(calls.len()) {
                Err(RuntimeError::not_started(
                    "planned_failure",
                    "planned failure",
                ))
            } else {
                Ok(ToolOutput::text(format!("call {}", calls.len())))
            }
        }

        async fn cleanup(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn direct_calls_share_one_runtime_and_stop_after_an_error() {
        let runtime = FakeRuntime {
            calls: Mutex::new(Vec::new()),
            fail_at: Some(2),
        };
        let calls = vec![
            json!({"name":"list_applications","arguments":{"scope":"running"}}),
            json!({"name":"observe","arguments":{"target":"Editor"}}),
            json!({"name":"list_applications","arguments":{"scope":"running"}}),
        ];
        let mut output = Vec::new();

        let error = execute_calls(&runtime, calls, &mut output)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("remaining calls were not executed")
        );
        assert_eq!(runtime.calls.lock().unwrap().len(), 2);
        let results = String::from_utf8(output).unwrap();
        let results = results
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["isError"], false);
        assert_eq!(results[1]["isError"], true);
    }

    #[test]
    fn direct_call_names_use_the_same_exact_matching_as_mcp() {
        let (name, _) = parse_call(json!({"name":" list_apps ","arguments":{}}), 0).unwrap();
        assert!(validate_call(&name, JsonObject::new()).is_err());
    }
}
