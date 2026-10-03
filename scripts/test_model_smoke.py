import copy
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
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
