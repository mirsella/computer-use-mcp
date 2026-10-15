#!/usr/bin/env python3
"""Optional model smoke against the local plugin and its private desktop."""

import argparse
import base64
import collections
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
MUTATIONS = {"launch_application", "activate_window", "act"}
DESKTOP_ACTIONS = MUTATIONS | {"list_desktop", "observe", "wait_for"}


def human_input_busy(part):
    state = part["state"]
    if (
        part["tool"] != "computer_use_dispatch"
        or state["status"] != "error"
        or state["input"]["action"] not in MUTATIONS
    ):
        return False
    lines = (state.get("output", "") + "\n" + str(state.get("error", ""))).splitlines()
    # The renderer's footer follows the message, which may quote arbitrary text.
    return (
        next((line for line in reversed(lines) if line.startswith("Code: ")), None)
        == "Code: HumanInputBusy"
    )


def read_events(stream):
    """Read complete JSONL records, leaving a partial record for the next poll."""
    while True:
        position = stream.tell()
        line = stream.readline()
        if not line.endswith(b"\n"):
            stream.seek(position)
            return
        yield json.loads(line)


def screenshots(state):
    return (
        item["uri"]
        for item in state.get("metadata", {}).get("content", [])
        if item.get("type") == "file" and item.get("mime") == "image/png"
    )


def summarize(events, phrase, task="editor"):
    desktop = "foreground" if task == "takeover" else "background"
    calls = [event["part"] for event in events if event["type"] == "tool_use"]
    assert calls, "model made no tool calls"
    assert any(
        part["tool"] == "skill"
        and part["state"]["input"].get("id") == "computer-use-mcp"
        for part in calls
    ), "computer-use skill was not loaded"
    errors, waits, actions = [], [], set()
    dispatches = []
    other = "foreground" if desktop == "background" else "background"
    for part in calls:
        tool, state = part["tool"], part["state"]
        assert tool in {"skill", "computer_use_help", "computer_use_dispatch"}, tool
        assert state["status"] in {"completed", "error"}, "unfinished tool call"
        output = state.get("output", "")
        assert f"Desktop: {other}" not in output, f"unexpected {other} result"
        if state["status"] == "error" or "Outcome: not_started" in output:
            errors.append(
                {
                    "tool": tool,
                    "input": state["input"],
                    "error": state.get("error", output),
                }
            )
        if tool != "computer_use_dispatch":
            continue
        action, args = state["input"]["action"], state["input"]["arguments"]
        if action in {"list_desktop", "launch_application"} or (
            action == "wait_for"
            and "target" not in args
            and args["condition"]["type"] == "window_opened"
        ):
            default = "foreground" if desktop == "foreground" else None
            assert args.get("desktop", default) == desktop, (
                f"non-{desktop} call: {state['input']}"
            )
        actions.add(action)
        dispatches.append(part)
        if action == "wait_for":
            waits.append(output)
    assert "launch_application" in actions and "act" in actions, (
        "editor task was not attempted"
    )
    final = dispatches[-1]["state"]
    assert final["input"]["action"] == "observe", (
        "verification must be the final desktop operation"
    )
    assert f"Desktop: {desktop}" in final["output"]
    readback = f'value="{phrase}"' in final["output"]
    if task == "calculator":
        readback = readback or f'name="{phrase}"' in final["output"]
    assert readback, "fresh accessibility readback did not match"
    image_uri = next(screenshots(final), None)
    assert image_uri, "fresh observation has no screenshot"
    prefix = "data:image/png;base64,"
    assert image_uri.startswith(prefix), "screenshot is not inline PNG data"
    image = base64.b64decode(image_uri.removeprefix(prefix), validate=True)
    assert image.startswith(b"\x89PNG\r\n\x1a\n"), "invalid screenshot"
    finishes = [event["part"] for event in events if event["type"] == "step_finish"]
    completed_text = (
        events[-1].get("type") == "text"
        and events[-1].get("part", {}).get("time", {}).get("end") is not None
    )
    assert not any(event.get("type") == "error" for event in events), "model transport failed"
    assert completed_text, "model did not finish normally"
    tokens = collections.Counter()
    for part in finishes:
        usage = part.get("tokens", {})
        for key in ("input", "output", "reasoning"):
            tokens[key] += usage.get(key, 0)
        for key in ("read", "write"):
            tokens[f"cache_{key}"] += usage.get("cache", {}).get(key, 0)
    repeated = collections.Counter(
        json.dumps(part["state"]["input"], sort_keys=True)
        for part in calls
        if part["tool"] == "computer_use_dispatch"
    )
    report = {
        "session_id": events[0].get("sessionID"),
        "tool_calls": len(calls),
        "steps": len(finishes),
        "tokens": dict(tokens),
        "repeated_calls": sum(count - 1 for count in repeated.values()),
        "tool_errors": errors,
        "wait_results": waits,
        "background_only": desktop == "background",
        "exact_readback": True,
        "screenshot_bytes": len(image),
    }
    if task == "capabilities":
        # Native call completion cannot establish these application effects.
        # Keep the model's assessment for review without declaring broad success.
        report.update(
            task_complete=False,
            verification_scope="editor_readback_only",
            unverified_capabilities=[
                "calculator_arithmetic", "window_controls", "target_window_crop",
                "semantic_actions", "pointer_gestures", "keyboard_and_paste", "waits",
            ],
            model_assessment=events[-1]["part"]["text"],
        )
    if desktop == "foreground":
        busy = next(
            (i for i, part in enumerate(dispatches) if human_input_busy(part)), None
        )
        assert busy is not None, "no actual HumanInputBusy tool refusal observed"
        idle = next(
            (
                i
                for i, part in enumerate(dispatches)
                if part["state"]["input"]["action"] == "wait_for"
                and part["state"]["input"]["arguments"]["condition"]["type"]
                == "human_idle"
            ),
            None,
        )
        assert idle is not None, "model did not wait for human_idle"
        assert idle > busy, "idle wait did not follow the refusal"
        idle_state = dispatches[idle]["state"]
        assert (
            idle_state["status"] == "completed"
            and "physical_monitor_unavailable" in idle_state["output"]
        ), "an isolated runner must not claim physical hardware idle"
        assert not any(
            part["state"]["input"]["action"] in MUTATIONS
            for part in dispatches[busy + 1 : idle]
        ), "model retried a mutation before waiting"
        first_act = next(
            (
                i
                for i, part in enumerate(dispatches)
                if i > idle and part["state"]["input"]["action"] == "act"
            ),
            None,
        )
        assert first_act is not None, "model did not resume input after waiting"
        assert any(
            part["state"]["input"]["action"] == "observe"
            for part in dispatches[idle + 1 : first_act]
        ), "model resumed input without a fresh observation"
        elapsed = (idle_state["time"]["end"] - idle_state["time"]["start"]) / 1000
        assert elapsed >= 59.5, f"quiet period returned too early: {elapsed}s"
        report.update(
            takeover_then_fresh_observation=True, idle_wait_seconds=round(elapsed, 1)
        )
    return report


def verify_history(events, records, phrase, task):
    calls = [
        event["part"]
        for event in events
        if event["type"] == "tool_use"
        and event["part"]["tool"] in {"computer_use_help", "computer_use_dispatch"}
    ]
    assert calls, "no public computer-use calls to verify"
    grouped = collections.defaultdict(list)
    for record in records:
        grouped[record["call_id"]].append(record)
    assert len(grouped) == len(calls), "missing calls or duplicate worker history"
    finished = []
    for pair in grouped.values():
        assert [record["event"] for record in pair] == ["started", "finished"], (
            "call is unmatched, duplicated, or abandoned"
        )
        start, end = pair
        assert all(
            end[key] == value for key, value in start.items() if key != "event"
        ), "terminal record lost its call metadata"
        assert end["duration_ms"] >= 0
        finished.append(end)
    assert len({record["connection_id"] for record in finished}) == 1, (
        "multiple brokers or worker-level history"
    )
    expected = collections.Counter(
        (
            part["tool"].removeprefix("computer_use_"),
            part["state"]["input"].get("action")
            if part["state"]["input"].get("action") in DESKTOP_ACTIONS else None,
            "error" if part["state"]["status"] == "error" else "succeeded",
        )
        for part in calls
    )
    actual = collections.Counter(
        (record["tool"], record.get("action"), record["status"]) for record in finished
    )
    assert actual == expected, "history operations or outcomes differ from tool results"
    desktop = "foreground" if task == "takeover" else "background"
    for record in finished:
        if record["tool"] == "dispatch":
            if "request" not in record:
                assert record["status"] == "error" and record.get("result", {}).get("code") == "invalid_arguments", "validated request metadata missing"
                assert record["result"].get("outcome") == "not_started", "invalid arguments dispatched input"
            else:
                assert record["desktop"] == desktop, "incorrect public desktop route"
    serialized = json.dumps(records)
    assert phrase not in serialized, "dummy text leaked into history"
    forbidden = {
        "arguments",
        "output",
        "text",
        "value",
        "message",
        "recovery",
        "data",
        "attachments",
    }

    def check_keys(value):
        if isinstance(value, dict):
            assert not forbidden & value.keys(), "history contains raw contents"
            for item in value.values():
                check_keys(item)
        elif isinstance(value, list):
            for item in value:
                check_keys(item)

    check_keys(records)
    for part in calls:
        for uri in screenshots(part["state"]):
            assert uri.split(",", 1)[1] not in serialized, "screenshot leaked into history"
    errors = collections.Counter(
        record["result"].get("code", "unknown")
        for record in finished
        if record["status"] == "error"
    )
    cleanup_failures = [
        record["call_id"]
        for record in finished
        if record["result"].get("action_progress", {}).get("cleanup") == "failed"
    ]
    longest = sorted(finished, key=lambda record: record["duration_ms"], reverse=True)[
        :5
    ]
    return {
        "calls": len(finished),
        "records": len(records),
        "error_codes": dict(errors),
        "failed_cleanup": cleanup_failures,
        "unmatched_starts": 0,
        "longest_calls": [
            {
                key: record[key]
                for key in ("call_id", "action", "duration_ms", "status")
                if key in record
            }
            for record in longest
        ],
    }


def capture_history(state, env):
    destination = state / "computer-use-mcp/history/calls.jsonl"
    destination.parent.mkdir(parents=True, mode=0o700)
    with destination.open("xb") as stream:
        os.fchmod(stream.fileno(), 0o600)
        subprocess.run(
            [str(ROOT / "vendor/bin/computer-use-mcp"), "history"],
            env=env,
            stdout=stream,
            check=True,
        )


def verify_history_filters(state, records):
    env = {**os.environ, "XDG_STATE_HOME": str(state)}

    def query(*arguments):
        output = subprocess.check_output(
            [str(ROOT / "vendor/bin/computer-use-mcp"), "history", *arguments],
            env=env,
        )
        return [json.loads(line) for line in output.splitlines()]

    last_id = records[-1]["call_id"]
    pair = [record for record in records if record["call_id"] == last_id]
    assert query("--last", "1") == pair, "--last split a call or selected wrong records"
    assert query("--call-id", last_id) == pair, "--call-id did not match exactly"
    assert query("--since", "15m") == records, "recent calls missing from --since"
    assert query("--errors") == [
        record for record in records if record.get("status") == "error"
    ]


Process = collections.namedtuple("Process", "parent start state name")


def read_process(pid):
    try:
        name, fields = (
            Path(f"/proc/{pid}/stat").read_text().split("(", 1)[1].rsplit(") ", 1)
        )
    except (FileNotFoundError, ProcessLookupError):
        return None
    fields = fields.split()
    return Process(int(fields[1]), fields[19], fields[0], name)


def isolation_markers(pid):
    try:
        environment = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        # KWin hides its environment; its supervisor still supplies the marker.
        return set()
    return {
        os.fsdecode(item.split(b"=", 1)[1])
        for item in environment
        if item.startswith(b"COMPUTER_USE_MCP_ISOLATION_MARKER=")
    }


class Processes:
    def __init__(self, pid, allow_foreground=False):
        process = read_process(pid)
        self.seen = {pid: process.start} if process else {}
        self.markers = set()
        self.allow_foreground = allow_foreground
        self.foreground_workers = set()

    def sample(self):
        table = {
            int(path.name): process
            for path in Path("/proc").glob("[0-9]*")
            if (process := read_process(path.name)) is not None
        }
        children = collections.defaultdict(list)
        for pid, process in table.items():
            children[process.parent].append(pid)
        owned = {
            pid
            for pid, start in self.seen.items()
            if pid in table and table[pid].start == start
        }
        pending = list(owned)
        while pending:
            for pid in children[pending.pop()]:
                if pid not in owned:
                    owned.add(pid)
                    pending.append(pid)
        for pid in owned:
            self.seen[pid] = table[pid].start
            try:
                argv = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                if Path(os.fsdecode(argv[0])).name == "computer-use-mcp":
                    assert Path(f"/proc/{pid}/exe").samefile(ROOT / "vendor/bin/computer-use-mcp"), (
                        "model MCP executable differs from the current checkout artifact; finish active tests before rebuilding or reconnect a stale server"
                    )
                    if b"__desktop_worker" in argv:
                        assert self.allow_foreground, "foreground worker started"
                        assert isolation_markers(pid), (
                            "worker lacks private runner ownership"
                        )
                        self.foreground_workers.add((pid, table[pid].start))
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                pass
            self.markers.update(isolation_markers(pid))
        return table

    def remaining(self):
        table = self.sample()
        # Detached applications may be reparented between samples.
        return {
            pid: process.name
            for pid, process in table.items()
            if process.state != "Z"
            and (
                self.seen.get(pid) == process.start
                or self.markers & isolation_markers(pid)
            )
        }


def read_providers(config):
    # debug config intentionally redacts headers and API keys; it cannot be
    # used as the source for another executable configuration.
    return json.loads(
        subprocess.check_output(
            [
                "bun",
                "--eval",
                "const config = Bun.JSONC.parse(await Bun.file(process.argv[1]).text()); "
                "console.log(JSON.stringify(config.providers ?? {}));",
                str(config),
            ],
            text=True,
        )
    )


def run(model, variant, timeout_seconds, provider_config, task="editor"):
    takeover = task == "takeover"
    home = Path.home()
    env = os.environ.copy()
    env["OPENCODE_DISABLE_PROJECT_CONFIG"] = "1"
    providers = read_providers(provider_config)
    # Outside the repository so project skills cannot mask the packaged skill.
    with tempfile.TemporaryDirectory(
        prefix="computer-use-model-", dir=ROOT.parent
    ) as temporary:
        work = Path(temporary)
        for directory in ("config", "runtime"):
            (work / directory).mkdir(mode=0o700)
        (work / "package.json").write_text('{"private":true}')
        config = {
            "$schema": "https://opencode.ai/config.json",
            "plugins": [ROOT.as_uri()],
            "providers": providers,
            "update": "disable",
            "share": "disabled",
            "permissions": [
                {"action": "*", "resource": "*", "effect": "deny"},
                {"action": "skill", "resource": "*", "effect": "allow"},
                {"action": "computer_use_*", "resource": "*", "effect": "allow"},
            ],
            "agents": {
                "desktop-test": {
                    "mode": "primary",
                    "description": "Private desktop release smoke",
                    "system": (
                        "Use the computer-use skill and tools on the foreground route of the "
                        "runner-owned private desktop. Do not select background. "
                        "This disposable private desktop is authorized for the dummy task."
                        if takeover
                        else "Use the computer-use skill and tools on the private background desktop only. Verify application effects from fresh observations."
                    ),
                }
            },
        }
        (work / "config/opencode.json").write_text(json.dumps(config))
        for key in (
            "OPENCODE_CONFIG",
            "OPENCODE_CLI_CONFIG_CONTENT",
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "COMPUTER_USE_MCP_TAKEOVER",
        ):
            env.pop(key, None)
        env.update(
            {
                "HOME": str(work),
                "OPENCODE_TEST_HOME": str(work),
                "OPENCODE_CONFIG_DIR": str(work / "config"),
                "OPENCODE_CONFIG_CONTENT": "{}",
                "OPENCODE_DB": str(work / "opencode.db"),
                "OPENCODE_DISABLE_FILEWATCHER": "1",
                "XDG_CONFIG_HOME": str(work / "config-home"),
                "XDG_STATE_HOME": str(work / "state"),
                "XDG_RUNTIME_DIR": str(work / "runtime"),
                "XDG_DATA_HOME": os.environ.get(
                    "XDG_DATA_HOME", str(home / ".local/share")
                ),
                "XDG_CACHE_HOME": os.environ.get(
                    "XDG_CACHE_HOME", str(home / ".cache")
                ),
            }
        )
        run_id = uuid.uuid4().hex[:12]
        phrase = f"OpenCode {task} smoke {run_id}"
        history_state = ROOT / "target/model-smoke" / f"{task}-{run_id}"
        history_state.mkdir(parents=True, mode=0o700)
        prompt = (
            "Use the computer-use skill, then open a graphical text editor on the private background desktop. "
            f"Create a new unsaved document containing exactly this line: {phrase}\n"
            "Verify it with a fresh observation containing both screenshot and accessibility readback. "
            "Use desktop=background explicitly on discovery, launch, and targetless window-open waits. "
            "Only use computer-use tools and skill. Do not use the foreground desktop or save a file. "
            "Leave the unsaved editor open for the test runner to clean up. Report the result and any recoveries."
        )
        if task == "calculator":
            prompt = (
                "Use the computer-use skill and the six advertised desktop actions only. "
                "On the private background desktop, discover and launch KCalc. "
                "Use its actual UI to compute 17*23, then verify the result 391 in a fresh "
                "observation containing both screenshot and accessibility. Do not merely "
                "write the answer into an editable field. Use an AT-SPI target for buttons "
                "or input, and only element IDs from the latest replacement observation. "
                "Use desktop=background explicitly on discovery, launch and targetless "
                "window-open waits. If an action name or shape is unclear, call help "
                "without action to list the six names, then fetch the relevant schema. "
                "Finish after the successful readback; the last desktop action must be "
                "observe with both views. Leave the private app for the runner to clean up."
            )
        if task == "capabilities":
            prompt += (
                " Before the final editor verification, exercise the advertised desktop capabilities "
                "using only your own private apps. Discover installed IDs; launch KCalc if available "
                "and verify 17*23=391 through actual UI readback. Test window-open, activation, "
                "minimize/maximize/restore, target_window screenshot cropping, and closure of a "
                "disposable clean window with window_closed. Select KWin window targets for controls "
                "and crops, and independently select AT-SPI targets for accessibility; never fuse "
                "identities by title or PID. Test semantic invoke, set_value and named actions only "
                "when advertised, pointer move/click/scroll/drag with fresh screenshot-grounded "
                "coordinates, keyboard and paste. Use disposable editor text to demonstrate visible "
                "scrolling and drag selection. Test frame_advanced, frame_changed, frame_stable, "
                "accessibility_advanced and element_value waits with correct retained baselines. "
                "A timeout or dispatch completion alone is not proof of application effect. "
                "Do not repeat impossible waits or unchanged unsupported calls. Report PASS, "
                "FAIL, UNSUPPORTED or UNTESTED per case, with actual evidence and your mistakes. "
                "Finally restore the editor to the exact requested line and make a fresh both-view "
                "observation the last desktop operation; leave that editor for runner cleanup."
            )
        handoff = work / "handoff"
        env["COMPUTER_USE_MCP_TAKEOVER_FILE"] = str(handoff)
        if takeover:
            handoff.touch()
            prompt = (
                "Use the computer-use skill and tools to create an unsaved dummy editor document "
                f"containing exactly this line: {phrase}\n"
                "Use the foreground route inside this owned private runner, never background. "
                "First discover and attempt to launch the editor. A cooperative handoff signal "
                "will refuse the launch with HumanInputBusy. The harness clears it after that refusal. "
                'Then call wait_for with condition {"type":"human_idle"} and timeout_ms 120000, '
                "with no target or desktop fields. Do not retry mutations before the wait returns. "
                "The verified isolated runner deliberately has no physical input watcher, so "
                "physical_monitor_unavailable is expected when the quiet period expires. "
                "You are authorized to continue the dummy task on this private desktop after that wait. "
                "Rediscover, launch, and obtain a fresh observation before entering text. "
                "End with a fresh observation containing screenshot and exact accessibility readback. "
                "Only use computer-use tools and skill. Do not save any file. "
                "Leave the unsaved editor for the harness to clean up. Report the refusal and recovery."
            )
        command = [
            "opencode",
            "run",
            "--standalone",
            "--model",
            f"{model}#{variant}",
            "--agent",
            "desktop-test",
            "--format",
            "json",
            "--title",
            "Computer-use release smoke",
            prompt,
        ]
        if takeover:
            auth = Path(env["XDG_DATA_HOME"]) / "opencode/auth.json"
            # The runner replaces writable XDG state. Copy authentication only
            # into its private data directory, which teardown removes.
            command = [
                str(ROOT / "scripts/run-isolated-session.sh"),
                "--width",
                "1280",
                "--height",
                "720",
                "--",
                sys.executable,
                "-B",
                "-c",
                "import os, runpy, shutil, subprocess, sys; from pathlib import Path; "
                "dest = Path(os.environ['XDG_DATA_HOME']) / 'opencode/auth.json'; "
                "dest.parent.mkdir(mode=0o700); shutil.copyfile(sys.argv[1], dest); "
                "dest.chmod(0o600); "
                "result = subprocess.run(['opencode'] + sys.argv[4:]); "
                "runpy.run_path(sys.argv[2])['capture_history'](Path(sys.argv[3]), os.environ.copy()); "
                "sys.exit(result.returncode);",
                str(auth),
                str(Path(__file__).resolve()),
                str(history_state),
                *command[1:],
            ]
        start = time.monotonic()
        with (
            (work / "events.jsonl").open("wb") as out,
            (work / "stderr.log").open("w") as err,
            (work / "events.jsonl").open("rb") as live_events,
        ):
            process = subprocess.Popen(
                command,
                cwd=work,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=err,
                start_new_session=True,
            )
            owned = Processes(process.pid, allow_foreground=takeover)
            try:
                while process.poll() is None:
                    owned.sample()
                    if takeover and handoff.exists():
                        # Only complete tool-result events authorize clearing
                        # the signal, never model prose or a partial JSON line.
                        if any(
                            event["type"] == "tool_use"
                            and human_input_busy(event["part"])
                            for event in read_events(live_events)
                        ):
                            handoff.unlink()
                    if time.monotonic() - start > timeout_seconds:
                        raise TimeoutError("model smoke exceeded its deadline")
                    time.sleep(0.25)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                deadline = time.monotonic() + 30
                while owned.remaining() and time.monotonic() < deadline:
                    time.sleep(0.25)
                remaining = owned.remaining()
                assert not remaining, f"owned processes survived teardown: {remaining}"
        assert process.returncode == 0, {
            "returncode": process.returncode,
            "stderr": (work / "stderr.log").read_text(),
            "events": (work / "events.jsonl").read_text()[-8_000:],
        }
        assert owned.markers, "no private runner isolation marker observed"
        assert all(not Path(marker).parent.exists() for marker in owned.markers), (
            "private runtime survived teardown"
        )
        with (work / "events.jsonl").open("rb") as stream:
            events = list(read_events(stream))
            assert not stream.read(), "unfinished JSON event at shutdown"
        if not takeover:
            capture_history(history_state, env)
        report = summarize(events, "391" if task == "calculator" else phrase, task)
        records = [
            json.loads(line)
            for line in (history_state / "computer-use-mcp/history/calls.jsonl")
            .read_bytes()
            .splitlines()
        ]
        report["history"] = verify_history(events, records, phrase, task)
        verify_history_filters(history_state, records)
        report["history_state_dir"] = str(history_state)
        if takeover:
            assert not handoff.exists(), "cooperative handoff was not exercised"
            assert len(owned.foreground_workers) == 1, "foreground worker restarted"
            report["resumed_same_worker"] = True
        report.update(
            model=model,
            task=task,
            elapsed_seconds=round(time.monotonic() - start, 1),
            foreground_workers=len(owned.foreground_workers),
            private_processes_remaining=0,
        )
        return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", default="openai/gpt-6.1-sol")
    parser.add_argument("--variant", default="medium")
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--task", choices=("editor", "takeover", "capabilities", "calculator"), default="editor")
    parser.add_argument(
        "--provider-config",
        type=Path,
        default=Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
        / "opencode/opencode.jsonc",
        help="JSON/JSONC file containing the provider settings for the test",
    )
    args = parser.parse_args()
    print(
        json.dumps(
            run(
                args.model, args.variant, args.timeout, args.provider_config, args.task
            ),
            indent=2,
        )
    )
