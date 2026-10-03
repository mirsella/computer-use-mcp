use std::{
    io::{Read, Write},
    time::Duration,
};

use serde_json::{Map as JsonObject, Value};

use crate::{
    VERSION,
    accessibility::AccessibilityAdapter,
    atspi_adapter::AtspiAdapter,
    errors::CliError,
    portal::{PortalApproval, PortalBackend, XdgPortalBackend, validate_capabilities},
    server,
};

const HELP: &str = "Computer Use MCP for Linux Wayland\n\nUsage:\n  computer-use-mcp [command]\n\nCommands:\n  init          Ask KDE to approve one monitor and save its restore token.\n  mcp           Serve six tools over stdio; --compact-tools exposes help/dispatch instead.\n  call FILE     Execute a call object or an array through one stateful desktop broker; use - for stdin.\n  doctor        Report Wayland, portal, PipeWire, AT-SPI, and input prerequisites without prompting.\n  help          Show this help.\n  version       Print the CLI version.\n\nCall input uses {\"name\":\"list_desktop\",\"arguments\":{\"scope\":\"windows\",\"desktop\":\"background\"}} objects and prints one standard MCP result per line. Discovery and launch accept desktop=foreground or background; returned IDs route later calls. Each CLI batch owns its sessions until exit. Run init only to approve foreground KDE access separately. KDE may ask again after revocation or display changes.\n";

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
            let compact = match arguments.get(1).map(String::as_str) {
                None => false,
                Some("--compact-tools") if arguments.len() == 2 => true,
                _ => {
                    return Err(CliError::InvalidArguments(
                        "mcp accepts only --compact-tools".to_owned(),
                    ));
                }
            };
            crate::broker::serve_stdio(compact).await
        }
        "__desktop_worker" => run_worker(&arguments, false).await,
        "__background_worker" => run_worker(&arguments, true).await,
        unknown => Err(CliError::InvalidCommand(unknown.to_owned())),
    }
}

async fn run_worker(arguments: &[String], require_isolation: bool) -> Result<(), CliError> {
    require_no_extra_arguments(arguments)?;
    if require_isolation {
        let session = crate::session::describe_session_from_env();
        if !session.isolated {
            return Err(CliError::Mcp(format!(
                "background worker isolation verification failed: {}",
                session.isolation_reason
            )));
        }
    }
    server::serve_worker_stdio().await
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

    let broker = crate::broker::DesktopBroker::new()?;
    let result = {
        let stdout = std::io::stdout();
        execute_broker_calls(&broker, calls, &mut stdout.lock()).await
    };
    broker.shutdown().await;
    result
}

async fn execute_broker_calls<W: Write>(
    broker: &crate::broker::DesktopBroker,
    calls: Vec<Value>,
    output: &mut W,
) -> Result<(), CliError> {
    for (index, value) in calls.into_iter().enumerate() {
        let (name, arguments) = parse_call(value, index)?;
        let result = broker.call(&name, arguments, std::future::pending()).await;
        serde_json::to_writer(&mut *output, &result)
            .map_err(|error| CliError::Mcp(format!("failed to write call result: {error}")))?;
        output
            .write_all(b"\n")
            .and_then(|_| output.flush())
            .map_err(|error| CliError::Mcp(format!("failed to flush call result: {error}")))?;
        if result.is_error == Some(true) {
            return Err(CliError::Mcp(format!(
                "call {} ({name}) failed; remaining calls were not executed",
                index + 1
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

    let session = crate::session::describe_session_from_env();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let display = session.display.clone();
    let wayland_ready = session_type.as_deref() == Some("wayland") && display.is_some();
    println!("\n[Wayland session]");
    print_doctor_status(wayland_ready);
    println!(
        "Session type: {}",
        session_type.as_deref().unwrap_or("<unset>")
    );
    println!("Display: {}", display.as_deref().unwrap_or("<unset>"));
    match session.socket.as_ref() {
        Some(socket) => println!(
            "Socket: {} ({})",
            socket.display(),
            if session.socket_present {
                "present"
            } else {
                "MISSING"
            }
        ),
        None => println!("Socket: <unresolvable>"),
    }
    println!(
        "Session isolation: {} ({})",
        if session.isolated {
            "ISOLATED virtual session"
        } else {
            "SHARED physical session"
        },
        session.isolation_reason
    );
    if !wayland_ready {
        println!("Action: run this command inside a KDE Plasma Wayland login session,");
        println!("or start an isolated virtual session with scripts/run-isolated-session.sh.");
    } else if !session.socket_present {
        println!("Action: the display socket is missing; re-enter the Wayland session or");
        println!("start an isolated virtual session with scripts/run-isolated-session.sh.");
    }

    println!("\n[Accessibility (AT-SPI)]");
    let atspi = AtspiAdapter::default();
    print_doctor_result(atspi.discover().await.map(|_| ()));

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

    // Read-only KWin virtual-desktop probe: snapshot only, never switches.
    // Fail-closed: a missing session bus reports UNAVAILABLE with its reason.
    println!("\n[Virtual desktop (KWin)]");
    match crate::virtual_desktop::VirtualDesktopProvider::live()
        .snapshot()
        .await
    {
        Some(Ok(snapshot)) => {
            print_doctor_status(true);
            println!("Current: {}", snapshot.summary_text());
            println!("Desktops: {}", snapshot.count);
        }
        Some(Err(error)) => {
            print_doctor_status(false);
            println!("Detail: {error}");
        }
        None => {
            print_doctor_status(false);
            println!("Detail: virtual desktop provider is disabled");
        }
    }

    println!("\n[Portal approval and EIS input]");
    println!("Status: NOT TESTED");
    println!("Reason: verifying monitor approval and EIS routing would require consent.");
    println!("Action: run `computer-use-mcp init`, then use the MCP server.");

    println!("\n[Human takeover]");
    let takeover_file = crate::takeover::takeover_file_path_from_env();
    let takeover_armed = crate::takeover::takeover_requested();
    println!(
        "Status: {}",
        if takeover_armed {
            "TAKEOVER REQUESTED"
        } else {
            "clear (no handoff signal)"
        }
    );
    match takeover_file.as_ref() {
        Some(path) => println!("Handoff file: {}", path.display()),
        None => println!("Handoff file: <none: set COMPUTER_USE_MCP_TAKEOVER_FILE>"),
    }
    println!("Env override: COMPUTER_USE_MCP_TAKEOVER (1 forces handoff)");
    match crate::takeover::hardware_watcher_status() {
        crate::takeover::HardwareWatcherStatus::Watching { devices } => println!(
            "Physical input watcher: watching {devices} input device(s) via /dev/input \
             (requires input group membership)"
        ),
        crate::takeover::HardwareWatcherStatus::Disabled { reason } => {
            println!("Physical input watcher: disabled ({reason})");
        }
    }
    if crate::takeover::device_paths_from_env().is_none() {
        println!(
            "Device override: COMPUTER_USE_MCP_INPUT_DEVICES (colon-separated paths, for tests)"
        );
    }
    println!("InputCapture portal monitoring: not used (pointer-barrier capture API, wrong tool);");
    println!("EIS physical-modifier refusals still surface as UserTakeoverInterrupted.");
    if takeover_armed {
        println!("Action: act and wait_for will refuse with UserTakeoverInterrupted until");
        println!("the handoff signal is cleared; held input is released first.");
    }
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
    use serde_json::json;

    use super::*;
    use crate::validation::validate_call;

    #[tokio::test]
    async fn direct_calls_stop_after_a_broker_validation_error() {
        let broker = crate::broker::DesktopBroker::new().unwrap();
        let calls = vec![
            json!({"name":"list_desktop","arguments":{"scope":"windows","desktop":"invalid"}}),
            json!({"name":"list_desktop","arguments":{"scope":"windows","desktop":"background"}}),
        ];
        let mut output = Vec::new();

        let error = execute_broker_calls(&broker, calls, &mut output)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("remaining calls were not executed")
        );
        let results = String::from_utf8(output).unwrap();
        let results = results
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["isError"], true);
        assert_eq!(results[0]["structuredContent"]["code"], "invalid_arguments");
        broker.shutdown().await;
    }

    #[test]
    fn direct_call_names_use_the_same_exact_matching_as_mcp() {
        let (name, _) = parse_call(json!({"name":" list_apps ","arguments":{}}), 0).unwrap();
        assert!(validate_call(&name, JsonObject::new()).is_err());
    }
}
