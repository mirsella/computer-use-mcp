import copy
import importlib.util
import io
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "model_smoke", Path(__file__).with_name("opencode-model-smoke.py")
)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def trace():
    def tool(name, arguments, output="", attachments=None):
        return {
            "type": "tool_use",
            "sessionID": "test-session",
            "part": {
                "tool": name,
                "state": {
                    "input": arguments,
                    "status": "completed",
                    "output": output,
                    "attachments": attachments or [],
                },
            },
        }

    return [
        tool("skill", {"name": "computer-use-mcp"}),
        tool(
            "computer_use_dispatch",
            {"action": "launch_application", "arguments": {"desktop": "background"}},
        ),
        tool("computer_use_dispatch", {"action": "act", "arguments": {}}),
        tool(
            "computer_use_dispatch",
            {"action": "observe", "arguments": {}},
            'Desktop: background\nvalue="exact smoke text"',
            [
                {
                    "mime": "image/png",
                    "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aXioAAAAASUVORK5CYII=",
                }
            ],
        ),
        {
            "type": "step_finish",
            "part": {
                "reason": "stop",
                "tokens": {
                    "input": 120,
                    "output": 30,
                    "reasoning": 10,
                    "cache": {"read": 80, "write": 4},
                },
            },
        },
    ]


class ModelSmokeEvidence(unittest.TestCase):
    @unittest.skipUnless(
        shutil.which("bun"), "optional provider JSONC check requires Bun"
    )
    def test_provider_config_preserves_headers_and_jsonc(self):
        with tempfile.TemporaryDirectory(dir=smoke.ROOT.parent) as directory:
            config = Path(directory) / "opencode.jsonc"
            config.write_text("""{
              // Provider settings must not come from redacted debug output.
              "provider": {"openai": {"options": {
                "apiKey": "test-key",
                "headers": {"sleev-provider": "test-provider"},
              }}},
            }""")
            options = smoke.read_providers(config)["openai"]["options"]
            self.assertEqual(options["apiKey"], "test-key")
            self.assertEqual(options["headers"]["sleev-provider"], "test-provider")

    def test_verified_trace_and_task_token_totals(self):
        events = trace()
        events.insert(-1, copy.deepcopy(events[-1]))
        report = smoke.summarize(events, "exact smoke text")
        self.assertTrue(report["exact_readback"])
        self.assertEqual(
            report["tokens"],
            {
                "input": 240,
                "output": 60,
                "reasoning": 20,
                "cache_read": 160,
                "cache_write": 8,
            },
        )

    def test_foreground_or_implicit_route_is_rejected(self):
        for arguments in [{}, {"desktop": "foreground"}]:
            events = trace()
            events[1]["part"]["state"]["input"]["arguments"] = arguments
            with self.assertRaisesRegex(AssertionError, "non-background"):
                smoke.summarize(events, "exact smoke text")

    def test_claim_without_fresh_readback_or_image_is_rejected(self):
        for field, value in [("output", "Done"), ("attachments", [])]:
            events = trace()
            events[3]["part"]["state"][field] = value
            with self.assertRaises(AssertionError):
                smoke.summarize(events, "exact smoke text")

    def test_mutation_after_verification_is_rejected(self):
        events = trace()
        events.insert(-1, copy.deepcopy(events[2]))
        with self.assertRaisesRegex(AssertionError, "final desktop operation"):
            smoke.summarize(events, "exact smoke text")

    def test_takeover_recovery_requires_real_refusal_wait_and_fresh_observation(self):
        events = trace()
        events[1]["part"]["state"]["input"]["arguments"]["desktop"] = "foreground"
        events[3]["part"]["state"]["output"] = (
            'Desktop: foreground\nvalue="exact smoke text"'
        )
        refusal = copy.deepcopy(events[1])
        refusal["part"]["state"].update(
            status="error", error="Code: HumanInputBusy\nOutcome: not_started"
        )
        idle = copy.deepcopy(events[1])
        idle["part"]["state"].update(
            input={
                "action": "wait_for",
                "arguments": {
                    "condition": {"type": "human_idle"},
                    "timeout_ms": 120000,
                },
            },
            output="Wait: human_idle satisfied=false reason=physical_monitor_unavailable",
            time={"start": 0, "end": 60000},
        )
        events[1:1] = [refusal, idle]
        events.insert(4, copy.deepcopy(events[-2]))
        report = smoke.summarize(events, "exact smoke text", "takeover")
        self.assertTrue(report["takeover_then_fresh_observation"])
        self.assertFalse(report["background_only"])
        self.assertEqual(report["idle_wait_seconds"], 60)

        missing_observation = copy.deepcopy(events)
        missing_observation.pop(4)
        with self.assertRaisesRegex(AssertionError, "fresh observation"):
            smoke.summarize(missing_observation, "exact smoke text", "takeover")
        premature_retry = copy.deepcopy(events)
        premature_retry.insert(2, copy.deepcopy(events[3]))
        with self.assertRaisesRegex(AssertionError, "before waiting"):
            smoke.summarize(premature_retry, "exact smoke text", "takeover")
        claim = copy.deepcopy(refusal["part"])
        claim["tool"] = "skill"
        claim["state"]["output"] = "Code: HumanInputBusy"
        self.assertFalse(smoke.human_input_busy(claim))

        short_wait = copy.deepcopy(events)
        short_wait[2]["part"]["state"]["time"]["end"] = 59000
        with self.assertRaisesRegex(AssertionError, "too early"):
            smoke.summarize(short_wait, "exact smoke text", "takeover")

    def test_only_failed_mutations_can_authorize_handoff_clearance(self):
        part = trace()[1]["part"]
        part["state"].update(
            status="error", error="Code: HumanInputBusy\nOutcome: not_started"
        )
        self.assertTrue(smoke.human_input_busy(part))
        for action, status, error in [
            ("observe", "error", "Code: HumanInputBusy"),
            ("act", "completed", "Code: HumanInputBusy"),
            ("act", "error", 'value="Code: HumanInputBusy"'),
            (
                "act",
                "error",
                "Message: quoted error\nCode: HumanInputBusy\nCode: backend_failed",
            ),
        ]:
            with self.subTest(action=action, status=status, error=error):
                part["state"]["input"]["action"] = action
                part["state"].update(status=status, error=error)
                self.assertFalse(smoke.human_input_busy(part))

    def test_live_event_reader_leaves_partial_records_and_does_not_replay(self):
        stream = io.BytesIO(b'{"type":"step_finish"}\n{"type":"tool_use"')
        self.assertEqual(list(smoke.read_events(stream)), [{"type": "step_finish"}])
        position = stream.tell()
        self.assertEqual(list(smoke.read_events(stream)), [])
        self.assertEqual(stream.tell(), position)
        stream.seek(0, io.SEEK_END)
        stream.write(b"}\n")
        stream.seek(position)
        self.assertEqual(list(smoke.read_events(stream)), [{"type": "tool_use"}])
        self.assertEqual(list(smoke.read_events(stream)), [])

        record = '{"value":"λ🙂"}\n'.encode()
        split = record.index("λ".encode()) + 1
        stream = io.BytesIO(record[:split])
        self.assertEqual(list(smoke.read_events(stream)), [])
        self.assertEqual(stream.tell(), 0)
        stream.seek(0, io.SEEK_END)
        stream.write(record[split:])
        stream.seek(0)
        self.assertEqual(list(smoke.read_events(stream)), [{"value": "λ🙂"}])

    def test_tracks_owned_process_tree_and_teardown(self):
        marker = f"model-smoke-test-{os.getpid()}"
        child_code = "import sys; sys.stdin.read()"
        parent_code = f"""
import subprocess, sys
with subprocess.Popen([sys.executable, '-c', {child_code!r}], stdin=subprocess.PIPE) as child:
    print(child.pid, flush=True)
    sys.stdin.read()
"""
        with subprocess.Popen(
            [sys.executable, "-c", parent_code],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            env={**os.environ, "COMPUTER_USE_MCP_ISOLATION_MARKER": marker},
        ) as parent:
            child = int(parent.stdout.readline())
            owned = smoke.Processes(parent.pid)
            self.assertEqual(set(owned.remaining()), {parent.pid, child})
            self.assertEqual(owned.markers, {marker})
            parent.communicate(timeout=5)
        self.assertEqual(owned.remaining(), {})


if __name__ == "__main__":
    unittest.main()
