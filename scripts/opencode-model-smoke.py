#!/usr/bin/env python3
"""Optional paid model smoke against the local plugin and its private desktop."""

import argparse
import base64
import collections
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def summarize(events, phrase):
    calls = [event["part"] for event in events if event["type"] == "tool_use"]
    assert calls, "model made no tool calls"
    assert any(
        part["tool"] == "skill"
        and part["state"]["input"].get("name") == "computer-use-mcp"
        for part in calls
    ), "computer-use skill was not loaded"
    errors, waits, actions = [], [], set()
    final = None
    for part in calls:
        tool, state = part["tool"], part["state"]
        assert tool in {"skill", "computer_use_help", "computer_use_dispatch"}, tool
        assert state["status"] in {"completed", "error"}, "unfinished tool call"
        output = state.get("output", "")
        assert "Desktop: foreground" not in output, "foreground result"
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
            assert args.get("desktop") == "background", (
                f"non-background call: {state['input']}"
            )
        actions.add(action)
        final = state
        if action == "wait_for":
            waits.append(output)
    assert "launch_application" in actions and "act" in actions, (
        "editor task was not attempted"
    )
    assert final["input"]["action"] == "observe", (
        "verification must be the final desktop operation"
    )
    assert "Desktop: background" in final["output"]
    assert f'value="{phrase}"' in final["output"], (
        "fresh accessibility readback did not match"
    )
    images = [
        item for item in final.get("attachments", []) if item.get("mime") == "image/png"
    ]
    assert images, "fresh observation has no screenshot"
    image = base64.b64decode(images[0]["url"].split(",", 1)[1], validate=True)
    assert image.startswith(b"\x89PNG\r\n\x1a\n"), "invalid screenshot"
    finishes = [event["part"] for event in events if event["type"] == "step_finish"]
    assert finishes and finishes[-1]["reason"] == "stop", (
        "model did not finish normally"
    )
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
    return {
        "session_id": events[0].get("sessionID"),
        "tool_calls": len(calls),
        "steps": len(finishes),
        "tokens": dict(tokens),
        "repeated_calls": sum(count - 1 for count in repeated.values()),
        "tool_errors": errors,
        "window_waits": waits,
        "background_only": True,
        "exact_readback": True,
        "screenshot_bytes": len(image),
    }


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
    def __init__(self, pid):
        process = read_process(pid)
        self.seen = {pid: process.start} if process else {}
        self.markers = set()

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
                    assert b"__desktop_worker" not in argv, "foreground worker started"
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


def run(model, variant, timeout_seconds):
    home = Path.home()
    env = os.environ.copy()
    env["OPENCODE_DISABLE_PROJECT_CONFIG"] = "1"
    providers = json.loads(
        subprocess.check_output(["opencode", "debug", "config"], env=env, text=True)
    ).get("provider", {})
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
            "plugin": [(ROOT / "plugin/computer-use.mjs").as_uri()],
            "provider": providers,
            "autoupdate": False,
            "share": "disabled",
            "tools": {"*": False, "skill": True, "computer_use_*": True},
            "permission": {"*": "deny", "skill": "allow", "computer_use_*": "allow"},
            "agent": {
                "desktop-test": {
                    "mode": "primary",
                    "description": "Private desktop release smoke",
                    "prompt": "Use the computer-use skill and tools on the private background desktop only. Verify application effects from fresh observations.",
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
        resolved = json.loads(
            subprocess.check_output(
                ["opencode", "debug", "config"], cwd=work, env=env, text=True
            )
        )
        assert resolved["mcp"]["computer_use"]["command"] == [
            str(ROOT / "vendor/bin/computer-use-mcp"),
            "mcp",
            "--compact-tools",
        ]
        phrase = f"OpenCode background smoke {uuid.uuid4().hex[:12]}"
        prompt = (
            "Use the computer-use skill, then open a graphical text editor on the private background desktop. "
            f"Create a new unsaved document containing exactly this line: {phrase}\n"
            "Verify it with a fresh observation containing both screenshot and accessibility readback. "
            "Use desktop=background explicitly on discovery, launch, and targetless window-open waits. "
            "Only use computer-use tools and skill. Do not use the foreground desktop or save a file. "
            "Leave the unsaved editor open for the test runner to clean up. Report the result and any recoveries."
        )
        command = [
            "opencode",
            "run",
            "--model",
            model,
            "--variant",
            variant,
            "--agent",
            "desktop-test",
            "--format",
            "json",
            "--title",
            "Computer-use release smoke",
            prompt,
        ]
        start = time.monotonic()
        with (
            (work / "events.jsonl").open("w") as out,
            (work / "stderr.log").open("w") as err,
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
            owned = Processes(process.pid)
            try:
                while process.poll() is None:
                    owned.sample()
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
        assert process.returncode == 0, (work / "stderr.log").read_text()
        assert owned.markers, "no private runner isolation marker observed"
        assert all(not Path(marker).parent.exists() for marker in owned.markers), (
            "private runtime survived teardown"
        )
        events = [
            json.loads(line)
            for line in (work / "events.jsonl").read_text().splitlines()
        ]
        report = summarize(events, phrase)
        report.update(
            model=model,
            elapsed_seconds=round(time.monotonic() - start, 1),
            foreground_workers=0,
            private_processes_remaining=0,
        )
        return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", default="openai/gpt-6-sol")
    parser.add_argument("--variant", default="medium")
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()
    print(json.dumps(run(args.model, args.variant, args.timeout), indent=2))
