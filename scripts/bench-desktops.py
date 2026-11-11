#!/usr/bin/env python3
"""Matched, receiver-verified model pilot in independent owned KWin sessions.

Never contacts the physical display. Counts provider-reported tokens separately
from wire bytes. Credentials live only in a disposable private task directory.
"""

import argparse
from collections import Counter
from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
import random
import selectors
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from desktop_bench_workloads import WORKLOADS, workload

ROOT = Path(__file__).resolve().parent.parent
MODEL = "opencode-go/deepseek-v4.1-flash#max"
TASKS = {
    "easy": 'In Desktop Benchmark, replace Message with exactly "Wayland benchmark 42" and click Commit message.',
    "medium": "In Desktop Benchmark, confirm the inventory batch using the footer button below the 2000-row inventory. Do not select or edit rows.",
    "hard": 'In Desktop Benchmark, click Start shipment, enter Recipient exactly "Ada Lovelace", Dispatch note exactly "café Δ 42", enable Express delivery, click Review shipment and confirm in the confirmation dialog.',
    "visual": 'In Desktop Benchmark, use its drawing canvas: click the GREEN rectangle, type exactly "Wayland test42" using generated keyboard input (not set_value), then press Enter. The canvas intentionally exposes no semantic controls. Use actual screenshot pixels; never infer coordinates from accessibility bounds.',
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def rows(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def summarize(events, journal):
    steps = [event["part"] for event in events if event.get("type") == "step_finish"]
    tokens = [step.get("tokens", {}) for step in steps]
    calls = [row for row in journal if row["method"] == "tools/call"]
    # Host argument-validation failures never reach the proxy. Count the last
    # state of every model-side call too, without double-counting state updates.
    attempts = {}
    for index, event in enumerate(events):
        part = event.get("part", {})
        if event.get("type") == "tool_use" and part.get("tool", "").startswith(
            "desktop_"
        ):
            attempts[part.get("callID", part.get("id", index))] = part
    finished = [
        part
        for part in attempts.values()
        if part.get("state", {}).get("status") in ("completed", "error")
    ]
    model_tool_errors = sum(part["state"]["status"] == "error" for part in finished)
    # OpenCode input is uncached input; cache read/write are separate fields.
    contexts = [
        token.get("input", 0)
        + sum(token.get("cache", {}).get(key, 0) for key in ("read", "write"))
        for token in tokens
    ]
    return {
        "model_steps": len(steps),
        "tool_calls": len(calls),
        "finished_model_tool_attempts": len(finished),
        "unfinished_model_tool_attempts": len(attempts) - len(finished),
        "model_tool_errors": model_tool_errors,
        "model_tool_error_rate": model_tool_errors / len(finished)
        if finished
        else None,
        "help_calls": sum(row.get("name") == "help" for row in calls),
        "operation_calls_by_name": dict(
            Counter(
                row.get("arguments", {}).get("action")
                if row.get("name") == "dispatch"
                else row.get("name")
                for row in calls
                if row.get("name") not in (None, "help")
            )
        ),
        "tool_errors": sum(row["error"] for row in calls),
        "tool_error_rate": sum(row["error"] for row in calls) / len(calls)
        if calls
        else None,
        "tool_seconds": sum(row["seconds"] for row in calls),
        "tool_text_bytes": sum(row["text_bytes"] for row in calls),
        "tool_image_bytes": sum(row["image_bytes"] for row in calls),
        "wire_response_bytes": sum(row["response_bytes"] for row in calls),
        "catalog_bytes": sum(
            row["response_bytes"] for row in journal if row["method"] == "tools/list"
        ),
        "input_tokens": sum(token.get("input", 0) for token in tokens),
        "output_tokens": sum(token.get("output", 0) for token in tokens),
        "reasoning_tokens": sum(token.get("reasoning", 0) for token in tokens),
        "cache_read_tokens": sum(
            token.get("cache", {}).get("read", 0) for token in tokens
        ),
        "cache_write_tokens": sum(
            token.get("cache", {}).get("write", 0) for token in tokens
        ),
        "peak_reported_input_context_tokens": max(contexts, default=0),
        "cumulative_reported_input_tokens": sum(contexts),
        "model_errors": [
            event.get("error") for event in events if event.get("type") == "error"
        ],
    }


def terminate(process):
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def model_stop(exit_code, timed_out, model_errors):
    if timed_out:
        return "deadline"
    if exit_code != 0:
        return "model_exit_error"
    return "model_error" if model_errors else "model_finished"


@contextmanager
def benchmark_credentials():
    config = (
        Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
        / "opencode/opencode.jsonc"
    )
    auth = json.loads(
        subprocess.check_output(
            ["opencode", "auth", "export", "--standalone", "opencode-go"],
            timeout=30,
        )
    )
    auth.extend(
        account
        for account in json.loads(
            subprocess.check_output(
                ["opencode", "auth", "export", "--standalone", "opencode"],
                timeout=30,
            )
        )
        if account["active"]
    )
    providers = json.loads(
        subprocess.check_output(
            [
                "bun",
                "-e",
                "process.stdout.write(JSON.stringify(Bun.JSONC.parse(await Bun.file(process.argv[1]).text()).providers))",
                str(config),
            ],
            timeout=30,
        )
    )
    with tempfile.TemporaryDirectory(
        prefix="bench-credentials-", dir=ROOT / ".scratch"
    ) as temporary:
        credentials = Path(temporary) / "credentials.json"
        credentials.write_text(json.dumps({"providers": providers, "auth": auth}))
        credentials.chmod(0o600)
        yield credentials


def interval_seconds(intervals):
    merged = []
    for start, end in sorted(intervals):
        if end < start:
            raise ValueError("Negative timing interval")
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return sum(end - start for start, end in merged)


def timing_summary(provider, journal, started, ended, expected_steps=None):
    provider = [
        row
        for row in provider
        if row["ended_monotonic"] >= started and row["started_monotonic"] <= ended
    ]

    def intervals(items):
        return [
            (max(started, row["started_monotonic"]), min(ended, row["ended_monotonic"]))
            for row in items
            if "started_monotonic" in row
            and row["ended_monotonic"] >= started
            and row["started_monotonic"] <= ended
        ]

    provider_intervals = intervals(provider)
    tool_intervals = intervals(
        [row for row in journal if row["method"] == "tools/call"]
    )
    result = dict(
        provider_requests=len(provider),
        provider_seconds_union=interval_seconds(provider_intervals),
        native_tool_seconds_union=interval_seconds(tool_intervals),
        unattributed_seconds=max(
            0, ended - started - interval_seconds(provider_intervals + tool_intervals)
        ),
        start_to_first_provider_seconds=min(
            (row["started_monotonic"] - started for row in provider), default=None
        ),
        provider_transport_errors=sum(
            row.get("transport_error") is not None for row in provider
        ),
        provider_http_errors=sum((row.get("status") or 0) >= 400 for row in provider),
        stream_inspection_errors=sum(
            row.get("stream_inspection_error") is not None for row in provider
        ),
    )
    for field in (
        "headers_seconds",
        "first_body_seconds",
        "first_sse_seconds",
        "first_output_delta_seconds",
        "first_reasoning_delta_seconds",
    ):
        values = [row[field] for row in provider if row.get(field) is not None]
        result[field] = values
    streams = [row for row in provider if row.get("event_stream")]
    result["provider_stream_requests"] = len(streams)
    result["expected_model_steps"] = expected_steps
    result["capture_complete"] = (
        expected_steps is not None
        and expected_steps > 0
        and len(streams) >= expected_steps
        and not result["stream_inspection_errors"]
        and bool(result["first_output_delta_seconds"])
    )
    result["downstream_disconnects_after_finish"] = sum(
        bool(row.get("downstream_disconnected") and row.get("finish_event_seen"))
        for row in provider
    )
    return result


def capture_model_output(source, events_path, receipts_path, errors):
    try:
        buffer = b""
        count = 0
        with events_path.open("wb") as events, receipts_path.open("w") as receipts:
            while chunk := os.read(source.fileno(), 65536):
                events.write(chunk)
                events.flush()
                buffer += chunk
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    if not line.strip():
                        continue
                    event = json.loads(line)
                    count += 1
                    part = event.get("part", {})
                    receipts.write(
                        json.dumps(
                            dict(
                                index=count,
                                monotonic=time.monotonic(),
                                wall=time.time(),
                                type=event.get("type"),
                                call_id=part.get("callID"),
                                status=part.get("state", {}).get("status"),
                            )
                        )
                        + "\n"
                    )
                    receipts.flush()
            if buffer.strip():
                raise RuntimeError("Model output ended with incomplete JSONL")
    except BaseException as error:
        errors.append(error)


def inside(args):
    with ExitStack() as resources:
        inside_run(args, resources)
    path = args.output / "summary.json"
    if path.exists() and args.provider_timing:
        summary = json.loads(path.read_text())
        summary["timing"] = timing_summary(
            rows(args.output / "provider.jsonl"),
            rows(args.output / "mcp.jsonl"),
            summary["model_started_monotonic"],
            summary["model_ended_monotonic"],
            summary["model_steps"],
        )
        path.write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n")


def inside_run(args, resources):
    inside_started = time.monotonic()
    marker = Path(os.environ["COMPUTER_USE_MCP_ISOLATION_MARKER"])
    if (
        not marker.is_file()
        or os.environ.get("GDK_BACKEND") != "wayland"
        or os.environ.get("DISPLAY")
    ):
        raise RuntimeError("Verified private native Wayland runner required")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True, mode=0o700)
    secrets = json.loads(args.credentials.read_text())
    if args.provider_timing:
        from bench_provider_timing import ProviderTimingProxy

        settings = secrets["providers"]["opencode-go"]["settings"]
        proxy = resources.enter_context(
            ProviderTimingProxy(settings["baseURL"], output / "provider.jsonl")
        )
        settings["baseURL"] = proxy.url
        secrets["providers"]["opencode-go"].setdefault("models", {}).setdefault(
            "deepseek-v4.1-flash", {}
        ).setdefault("settings", {})["baseURL"] = proxy.url
    config_dir = Path(os.environ["XDG_RUNTIME_DIR"]) / "opencode-config"
    config_dir.mkdir(mode=0o700)
    ours = args.system != "upstream"
    command = [str(args.binary), "mcp"]
    if ours and args.ours_transport == "compact":
        command.append("--compact-tools")
    native = [
        sys.executable,
        str(ROOT / "scripts/bench-mcp-proxy.py"),
        "--journal",
        str(output / "mcp.jsonl"),
        "--",
        *command,
    ]
    guidance = (
        "Use list_desktop and prefer exact AT-SPI targets when exposed, then observe. If no accessible target is exposed, inspect explicit capabilities/reasons and use supported screenshot-grounded input; never join identities across backends. The runner already launched the fixture. Use foreground/default desktop: it is this verified private headless session, NOT the person's physical session. Do not launch another background desktop. Keep exact target and observation IDs; adopt replacement observations after actions. Prefer advertised semantic actions/set_value. For ordinary observations use accessibility.limits.max_nodes=40 initially. For targeted name search use accessibility.search if advertised; otherwise narrow with query or raise limits if needed. For easy, medium and hard tasks use view=accessibility initially and after actions; request screenshots only if semantics are insufficient. For the canvas use screenshots."
        if ours
        else "Use list_apps to discover Desktop Benchmark, then get_app_state. The runner already launched the fixture on this private headless Wayland session. Use recent element indices from this process. Prefer semantic element click and set_value. For ordinary observations use max_tree_nodes=40 initially. For easy, medium and hard tasks initially request include_screenshot=false; request screenshots only if semantics are insufficient. For the canvas request include_screenshot=true."
    )
    case = workload(args.task, args.seed) if args.suite == "expanded" else None
    if case:
        guidance = guidance.replace("Desktop Benchmark", case["app"])
        guidance = guidance.replace(
            "For easy, medium and hard tasks", "For semantic and document tasks"
        )
    config = {
        "$schema": "https://opencode.ai/config.json",
        "plugins": [],
        "providers": secrets["providers"],
        "update": "disable",
        "share": "disabled",
        "mcp": {
            "servers": {
                "desktop": {
                    "type": "local",
                    "command": native,
                    "enabled": True,
                    "codemode": False,
                    "timeout": {"startup": 30000, "catalog": 30000, "execution": 45000},
                }
            }
        },
        "permissions": [
            {"action": "*", "resource": "*", "effect": "deny"},
            {"action": "desktop_*", "resource": "*", "effect": "allow"},
        ],
        "agents": {
            "desktop-bench": {
                "mode": "primary",
                "description": "Private desktop benchmark",
                "system": "Use only the advertised desktop MCP tools. No filesystem, shell, code, network, delegation or other tools. GUI document saving is allowed only when explicitly requested. Never claim effect from dispatch alone. Do not repeatedly retry an unsupported capability. "
                + guidance,
            }
        },
    }
    configuration = config_dir / "opencode.json"
    configuration.write_text(json.dumps(config))
    configuration.chmod(0o600)
    env = os.environ.copy()
    for key in (
        "OPENCODE_CONFIG",
        "OPENCODE_CLI_CONFIG_CONTENT",
        "COMPUTER_USE_MCP_BIN",
        "COMPUTER_USE_MCP_TAKEOVER",
        "OPEN_COMPUTER_USE_ACTION_READ_BACK",
        "OPEN_COMPUTER_USE_LINUX_LIGHTWEIGHT_HIDDEN",
        "OPENCODE_API_KEY",
    ):
        env.pop(key, None)
    env.update(
        OPENCODE_DISABLE_PROJECT_CONFIG="1",
        OPENCODE_CONFIG_DIR=str(config_dir),
        OPENCODE_CONFIG_CONTENT="{}",
        OPENCODE_DB=str(config_dir / "opencode.db"),
        OPENCODE_DISABLE_FILEWATCHER="1",
        OPENCODE_TEST_HOME=str(config_dir),
        HOME=str(config_dir),
    )
    # V2 credentials live in its database; auth.json is a legacy V1 store.
    # Go can use the active Console OAuth account instead of its API key.
    # Preserve that account selection in this disposable private database.
    imported = subprocess.run(
        ["opencode", "auth", "import", "--standalone"],
        input=json.dumps(secrets["auth"]).encode(),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        timeout=30,
    )
    if imported.returncode:
        raise RuntimeError(
            "Could not import selected provider into private OpenCode database"
        )
    started = time.monotonic()
    state_file = output / "receiver.json"
    with (output / "fixture.stderr").open("wb") as fixture_err:
        fixture = subprocess.Popen(
            [
                sys.executable,
                str(
                    ROOT
                    / "scripts/fixtures"
                    / ("deep-desktop-bench.py" if case else "desktop-bench.py")
                ),
                "--task",
                args.task,
                "--state",
                str(state_file),
                *(["--seed", str(args.seed)] if case else []),
            ],
            stdout=subprocess.PIPE,
            stderr=fixture_err,
            env=env,
        )
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(fixture.stdout, selectors.EVENT_READ)
                if not selector.select(15) or fixture.stdout.readline() != b"READY\n":
                    raise RuntimeError(
                        "Wayland fixture failed readiness; inspect fixture.stderr"
                    )
            fixture_ready = time.monotonic()
            goal = f"In {case['app']}, {case['goal']}" if case else TASKS[args.task]
            save_rule = (
                "Only save the existing benchmark document."
                if args.task == "kwrite"
                else "Do not save files."
            )
            prompt = (
                goal
                + " Verify the actual result with fresh UI evidence; stop when complete. "
                + save_rule
                + " Do not close the app. If the necessary Wayland capability is unsupported, report that honestly and stop. Stay within 60 desktop tool calls."
            )
            model_started = time.monotonic()
            with (
                (output / "opencode.stderr").open("wb") as stderr,
            ):
                model = subprocess.Popen(
                    [
                        "opencode",
                        "run",
                        "--standalone",
                        "--model",
                        MODEL,
                        "--agent",
                        "desktop-bench",
                        "--format",
                        "json",
                        "--title",
                        f"Wayland bench {args.system} {args.task}",
                        prompt,
                    ],
                    cwd=config_dir,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=stderr,
                )
                reader_errors = []
                reader = threading.Thread(
                    target=capture_model_output,
                    args=(
                        model.stdout,
                        output / "events.jsonl",
                        output / "event-receipts.jsonl",
                        reader_errors,
                    ),
                    name="model-event-recorder",
                )
                reader.start()
                timed_out = False
                try:
                    model.wait(timeout=args.timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    terminate(model)
                finally:
                    terminate(model)
                    reader.join()
                    model.stdout.close()
                if reader_errors:
                    raise RuntimeError(
                        "Model event recording failed"
                    ) from reader_errors[0]
                model_seconds = time.monotonic() - model_started
            receiver = json.loads(state_file.read_text())
            summary = summarize(
                rows(output / "events.jsonl"), rows(output / "mcp.jsonl")
            )
            stop = model_stop(model.returncode, timed_out, summary["model_errors"])
            summary.update(
                system=args.system,
                task=args.task,
                repetition=args.repetition,
                suite=args.suite,
                seed=args.seed,
                workload=case,
                model=MODEL,
                model_seconds=model_seconds,
                run_seconds=time.monotonic() - started,
                setup_seconds=started - inside_started,
                fixture_startup_seconds=fixture_ready - started,
                model_started_monotonic=model_started,
                model_ended_monotonic=model_started + model_seconds,
                success=receiver["success"],
                receiver=receiver,
                stop=stop,
                exit_code=model.returncode,
                binary_sha256=digest(args.binary),
                transport=args.ours_transport if ours else "direct",
                build_profile=(
                    args.updated_build_profile
                    if args.system == "updated"
                    else "debug"
                    if ours
                    else "default Go"
                ),
                native_wayland=True,
                isolation_marker=str(marker),
                private_runtime_removed=False,
            )
            (output / "summary.json").write_text(
                json.dumps(summary, indent=2, ensure_ascii=False) + "\n"
            )
        finally:
            terminate(fixture)
            fixture.stdout.close()


def write_provenance(
    output,
    binaries,
    *,
    collected_after_run=False,
    ours_transport="direct",
    updated_build_profile="debug",
):
    """Keep declared checkout state separate from measured executable hashes."""
    checkouts = {
        "baseline": ROOT / ".scratch/worktrees/bench-baseline",
        "updated": ROOT,
        "upstream": ROOT / ".scratch/research/open-computer-use",
    }
    sources = {}
    for system, binary in binaries.items():
        checkout = checkouts[system]
        with binary.open("rb") as executable:
            digest = hashlib.file_digest(executable, "sha256").hexdigest()
        sources[system] = {
            "binary_sha256": digest,
            "transport": ours_transport if system != "upstream" else "direct",
            "build_profile": (
                updated_build_profile
                if system == "updated"
                else "debug"
                if system == "baseline"
                else "default Go"
            ),
            "checkout_revision": subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=checkout, text=True
            ).strip(),
            "tracked_changes": subprocess.check_output(
                ["git", "diff", "--name-only", "HEAD"], cwd=checkout, text=True
            ).splitlines(),
        }
    if "updated" in binaries:
        patch = subprocess.check_output(
            [
                "git",
                "diff",
                "--binary",
                "HEAD",
                "--",
                "Cargo.toml",
                "Cargo.lock",
                "crates",
            ],
            cwd=ROOT,
        )
        (output / "updated-native-source.patch").write_bytes(patch)
    (output / "provenance.json").write_text(
        json.dumps(
            {
                "model": MODEL,
                "collected_after_run": collected_after_run,
                "sources": sources,
            },
            indent=2,
        )
        + "\n"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--baseline",
        type=Path,
        default=ROOT
        / ".scratch/worktrees/bench-baseline/target/debug/computer-use-mcp",
    )
    parser.add_argument(
        "--updated", type=Path, default=ROOT / "target/debug/computer-use-mcp"
    )
    parser.add_argument(
        "--upstream",
        type=Path,
        default=ROOT / ".scratch/research/open-computer-use/ocu-linux",
    )
    parser.add_argument(
        "--tasks", nargs="+", choices=sorted(set(TASKS) | set(WORKLOADS))
    )
    parser.add_argument("--suite", choices=("pilot", "expanded"), default="pilot")
    parser.add_argument("--seed-base", type=int, default=101)
    parser.add_argument("--seed", type=int, default=101)
    parser.add_argument("--provider-timing", action="store_true")
    parser.add_argument(
        "--systems",
        nargs="+",
        choices=("baseline", "updated", "upstream"),
    )
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument(
        "--ours-transport",
        choices=("direct", "compact"),
        default="direct",
        help="Compact matches our plugin default; direct preserves the full eager catalog",
    )
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument(
        "--updated-build-profile",
        choices=("debug", "release", "custom"),
        default="debug",
        help="Declared build profile of --updated; recorded, not inferred from its path",
    )
    parser.add_argument("--inside", action="store_true")
    parser.add_argument("--credentials", type=Path)
    parser.add_argument("--system")
    parser.add_argument("--task")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--repetition", type=int, default=1)
    args = parser.parse_args()
    os.umask(0o077)
    if args.inside:
        inside(args)
        return
    args.systems = args.systems or (
        ["updated", "upstream"]
        if args.suite == "expanded"
        else ["baseline", "updated", "upstream"]
    )
    if args.suite == "expanded" and "baseline" in args.systems:
        parser.error("expanded reporting supports only updated and upstream systems")
    args.tasks = args.tasks or list(WORKLOADS if args.suite == "expanded" else TASKS)
    allowed = WORKLOADS if args.suite == "expanded" else TASKS
    if not set(args.tasks) <= set(allowed) or args.repetitions < 1 or args.timeout < 1:
        raise ValueError("Invalid suite tasks, repetitions or deadline")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    if (args.output / "results.json").exists() or (
        args.output / "provenance.json"
    ).exists():
        raise RuntimeError(
            "Use a new output directory; previous evidence must not be overwritten"
        )
    binaries = {name: getattr(args, name).resolve() for name in args.systems}
    if not all(
        path.is_file() and os.access(path, os.X_OK) for path in binaries.values()
    ):
        raise RuntimeError(
            "Build every requested native executable before benchmarking"
        )
    write_provenance(
        args.output,
        binaries,
        ours_transport=args.ours_transport,
        updated_build_profile=args.updated_build_profile,
    )
    schedule = []
    for repetition in range(1, args.repetitions + 1):
        tasks = list(args.tasks)
        if args.suite == "expanded":
            random.Random(f"expanded-schedule:{args.seed_base}:{repetition}").shuffle(
                tasks
            )
        for index, task in enumerate(tasks):
            offset = (index + repetition - 1) % len(args.systems)
            seed = args.seed_base + repetition - 1
            for system in args.systems[offset:] + args.systems[:offset]:
                case = workload(task, seed) if args.suite == "expanded" else None
                schedule.append(
                    dict(
                        repetition=repetition,
                        task=task,
                        system=system,
                        seed=seed,
                        timeout=min(args.timeout, case["timeout_cap"])
                        if case
                        else args.timeout,
                    )
                )
    manifest = dict(
        suite=args.suite,
        model=MODEL,
        provider_timing=args.provider_timing,
        schedule=schedule,
        harness_sha256=digest(Path(__file__)),
        fixture_sha256=digest(
            ROOT
            / "scripts/fixtures"
            / (
                "deep-desktop-bench.py"
                if args.suite == "expanded"
                else "desktop-bench.py"
            )
        ),
        workload_sha256=digest(ROOT / "scripts/desktop_bench_workloads.py"),
        timing_sha256=digest(ROOT / "scripts/bench_provider_timing.py"),
    )
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    with benchmark_credentials() as credentials:
        summaries = []
        for trial in schedule:
            repetition, task, system = (
                trial[key] for key in ("repetition", "task", "system")
            )
            output = args.output / f"{repetition}-{task}-{system}"
            output.mkdir(mode=0o700)
            command = [
                str(ROOT / "scripts/run-isolated-session.sh"),
                "--width",
                "1280",
                "--height",
                "800",
                "--",
                sys.executable,
                str(Path(__file__).resolve()),
                "--inside",
                "--output",
                str(output),
                "--credentials",
                str(credentials),
                "--system",
                system,
                "--task",
                task,
                "--binary",
                str(binaries[system]),
                "--timeout",
                str(trial["timeout"]),
                "--repetition",
                str(repetition),
                "--ours-transport",
                args.ours_transport,
                "--updated-build-profile",
                args.updated_build_profile,
                "--suite",
                args.suite,
                "--seed",
                str(trial["seed"]),
                *(["--provider-timing"] if args.provider_timing else []),
            ]
            with (output / "runner.log").open("wb") as log:
                process = subprocess.Popen(command, stdout=log, stderr=log)
                try:
                    process.wait(timeout=trial["timeout"] + 150)
                except subprocess.TimeoutExpired:
                    terminate(process)
                finally:
                    terminate(process)
            summary_path = output / "summary.json"
            if not summary_path.exists():
                summary = {
                    "system": system,
                    "task": task,
                    "repetition": repetition,
                    "success": False,
                    "stop": "harness_failure",
                    "exit_code": process.returncode,
                }
            else:
                summary = json.loads(summary_path.read_text())
                summary["private_runtime_removed"] = not Path(
                    summary["isolation_marker"]
                ).parent.exists()
                if not summary["private_runtime_removed"]:
                    raise RuntimeError(
                        "Private runner teardown left its runtime behind"
                    )
            summaries.append(summary)
            (args.output / "results.json").write_text(
                json.dumps(summaries, indent=2, ensure_ascii=False) + "\n"
            )
            print(
                json.dumps(
                    {
                        key: summary.get(key)
                        for key in (
                            "system",
                            "task",
                            "success",
                            "tool_calls",
                            "model_seconds",
                            "stop",
                        )
                    }
                ),
                flush=True,
            )


if __name__ == "__main__":

    def interrupted(_signum, _frame):
        raise SystemExit(143)

    signal.signal(signal.SIGTERM, interrupted)
    main()
