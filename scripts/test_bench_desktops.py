#!/usr/bin/env python3
"""Deterministic accounting and byte-transparent MCP proxy regressions."""

import base64
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
from contextlib import redirect_stderr, redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen
from pathlib import Path

from bench_provider_timing import ProviderTimingProxy, StreamEvidence
from bench_expanded_report import clean
from desktop_bench_workloads import WORKLOADS, workload

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location(
    "bench", ROOT / "scripts/bench-desktops.py"
)
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
report_spec = importlib.util.spec_from_file_location(
    "report", ROOT / "scripts/bench-report.py"
)
report = importlib.util.module_from_spec(report_spec)
report_spec.loader.exec_module(report)


class AccountingTests(unittest.TestCase):
    def test_model_exit_failures_and_deadlines_are_not_clean_completions(self):
        trial = dict(
            success=True,
            receiver=dict(success=True, wrong_submissions=0, unsafe_action=False),
            model_tool_errors=0,
            unfinished_model_tool_attempts=0,
            stop="model_finished",
            exit_code=0,
        )
        self.assertTrue(clean(trial))
        for code in (7, -15):
            self.assertEqual(bench.model_stop(code, False, []), "model_exit_error")
            self.assertFalse(clean(dict(trial, exit_code=code)))
            failed = dict(trial, stop=bench.model_stop(code, False, []), exit_code=code)
            self.assertFalse(clean(failed))
            self.assertEqual(report.aggregate([failed])["model_or_setup_errors"], 1)
        self.assertEqual(bench.model_stop(-15, True, ["interrupted"]), "deadline")
        self.assertEqual(bench.model_stop(0, False, ["error"]), "model_error")
        self.assertEqual(bench.model_stop(0, False, []), "model_finished")

    def test_expanded_defaults_are_reportable_and_baseline_is_rejected_early(self):
        with tempfile.TemporaryDirectory(dir=ROOT / ".scratch") as temporary:
            root = Path(temporary)
            binary = root / "executable"
            binary.write_text("test executable, never launched")
            binary.chmod(0o700)
            argv = [
                "bench-desktops.py",
                "--suite",
                "expanded",
                "--output",
                str(root / "results"),
                "--updated",
                str(binary),
                "--upstream",
                str(binary),
            ]
            with (
                patch.object(sys, "argv", argv),
                patch.object(
                    bench,
                    "write_provenance",
                    side_effect=RuntimeError("stop before collection"),
                ) as provenance,
            ):
                with self.assertRaisesRegex(RuntimeError, "stop before collection"):
                    bench.main()
                self.assertEqual(
                    set(provenance.call_args.args[1]), {"updated", "upstream"}
                )
            with (
                patch.object(sys, "argv", argv + ["--systems", "baseline"]),
                redirect_stderr(io.StringIO()),
            ):
                with self.assertRaises(SystemExit) as rejected:
                    bench.main()
                self.assertEqual(rejected.exception.code, 2)

    def test_resume_persists_adopted_final_trial_without_exporting_credentials(self):
        spec = importlib.util.spec_from_file_location(
            "bench_resume", ROOT / "scripts/bench_resume.py"
        )
        resume = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(resume)
        with tempfile.TemporaryDirectory(dir=ROOT / ".scratch") as temporary:
            directory = Path(temporary)
            trial = dict(system="updated", task="entry", repetition=1, seed=101)
            manifest = dict(
                suite="expanded",
                model=bench.MODEL,
                schedule=[trial],
                **{
                    name: "frozen"
                    for name in (
                        "harness_sha256",
                        "fixture_sha256",
                        "workload_sha256",
                        "timing_sha256",
                    )
                },
            )
            (directory / "manifest.json").write_text(json.dumps(manifest))
            (directory / "provenance.json").write_text(
                json.dumps(
                    dict(
                        sources={
                            name: dict(binary_sha256="frozen")
                            for name in ("updated", "upstream")
                        }
                    )
                )
            )
            (directory / "results.json").write_text("[]")
            case = directory / "1-entry-updated"
            case.mkdir()
            row = dict(
                trial,
                binary_sha256="frozen",
                isolation_marker=str(directory / "removed-runtime/isolation.ready"),
            )
            (case / "summary.json").write_text(json.dumps(row))
            argv = [
                "bench_resume.py",
                str(directory),
                "--updated",
                "unused",
                "--upstream",
                "unused",
                "--stopped-worker",
                "2147483647",
            ]
            with (
                patch.object(sys, "argv", argv),
                patch.object(resume.bench, "digest", return_value="frozen"),
                patch.object(resume, "assert_stopped") as stopped,
                patch.object(
                    resume.bench,
                    "benchmark_credentials",
                    side_effect=AssertionError("no missing trials"),
                ),
                redirect_stdout(io.StringIO()),
            ):
                resume.main()
            stopped.assert_called_once_with(case)
            self.assertEqual(
                json.loads((directory / "results.json").read_text()),
                [dict(row, private_runtime_removed=True)],
            )
            self.assertFalse((directory / "results.pending.json").exists())

    @unittest.skipUnless(
        os.environ.get("COMPUTER_USE_MCP_ISOLATED") == "1",
        "requires the owned private desktop runner",
    )
    def test_real_receiver_latches_forbidden_personal_and_inventory_changes(self):
        import gi

        gi.require_version("Gtk", "3.0")
        from gi.repository import Gtk, GLib

        spec = importlib.util.spec_from_file_location(
            "receiver_fixture", ROOT / "scripts/fixtures/deep-desktop-bench.py"
        )
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        native_main = Gtk.main

        def descendants(widget):
            yield widget
            if isinstance(widget, Gtk.Container):
                for child in widget.get_children():
                    yield from descendants(child)

        for task, violation in (
            ("settings", None),
            ("settings", "edit"),
            ("inventory", None),
            ("inventory", "select"),
            ("inventory", "edit"),
        ):
            with (
                self.subTest(task=task, violation=violation),
                tempfile.TemporaryDirectory(dir=ROOT / ".scratch") as temporary,
            ):
                state_file = Path(temporary) / "receiver.json"
                failures = []

                def exercise():
                    windows = [
                        window
                        for window in Gtk.Window.list_toplevels()
                        if window.get_title() == "Desktop Benchmark"
                    ]
                    try:
                        self.assertEqual(len(windows), 1)
                        widgets = list(descendants(windows[0]))
                        if task == "settings":
                            notebook = next(
                                widget
                                for widget in widgets
                                if isinstance(widget, Gtk.Notebook)
                            )
                            if violation:
                                personal = next(
                                    widget
                                    for widget in descendants(notebook.get_nth_page(0))
                                    if isinstance(widget, Gtk.Entry)
                                )
                                original = personal.get_text()
                                personal.set_text("forbidden")
                                personal.set_text(original)
                            notebook.set_current_page(1)
                            delivery = list(descendants(notebook.get_nth_page(1)))
                            next(
                                widget
                                for widget in delivery
                                if isinstance(widget, Gtk.Entry)
                            ).set_text(workload(task, 101)["params"]["text"])
                            next(
                                widget
                                for widget in delivery
                                if isinstance(widget, Gtk.CheckButton)
                            ).set_active(True)
                            next(
                                widget
                                for widget in delivery
                                if isinstance(widget, Gtk.Button)
                                and widget.get_label() == "Save"
                            ).clicked()
                        else:
                            tree = next(
                                widget
                                for widget in widgets
                                if isinstance(widget, Gtk.TreeView)
                            )
                            if violation == "select":
                                tree.get_selection().select_path(
                                    Gtk.TreePath.new_from_indices([0])
                                )
                                tree.get_selection().unselect_all()
                            elif violation == "edit":
                                tree.get_model()[0][0] = "forbidden"
                            next(
                                widget
                                for widget in widgets
                                if isinstance(widget, Gtk.Button)
                                and widget.get_label() == "Confirm inventory batch"
                            ).clicked()
                        result = json.loads(state_file.read_text())
                        self.assertEqual(result["unsafe_action"], violation is not None)
                        self.assertEqual(result["success"], violation is None)
                    except BaseException as error:
                        failures.append(error)
                    finally:
                        for window in windows:
                            window.destroy()
                    return False

                def run_loop():
                    GLib.idle_add(exercise)
                    native_main()

                with (
                    patch.object(
                        sys,
                        "argv",
                        [
                            "receiver_fixture",
                            "--task",
                            task,
                            "--seed",
                            "101",
                            "--state",
                            str(state_file),
                        ],
                    ),
                    patch.object(Gtk, "main", side_effect=run_loop),
                ):
                    fixture.main()
                if failures:
                    raise failures[0]

    def test_credential_scope_is_shared_private_and_removed_on_failure(self):
        for fail in (False, True):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / ".scratch").mkdir()
                exports = [
                    b'[{"account":"go"}]',
                    b'[{"account":"active","active":true},{"account":"inactive","active":false}]',
                    b'{"provider":{}}',
                ]
                with (
                    patch.object(bench, "ROOT", root),
                    patch.object(
                        bench.subprocess, "check_output", side_effect=exports
                    ) as output,
                ):
                    try:
                        with bench.benchmark_credentials() as credentials:
                            directory = credentials.parent
                            self.assertEqual(credentials.stat().st_mode & 0o777, 0o600)
                            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
                            self.assertEqual(
                                json.loads(credentials.read_text()),
                                {
                                    "providers": {"provider": {}},
                                    "auth": [
                                        {"account": "go"},
                                        {"account": "active", "active": True},
                                    ],
                                },
                            )
                            if fail:
                                raise RuntimeError("trial failed")
                    except RuntimeError as error:
                        self.assertTrue(fail)
                        self.assertEqual(str(error), "trial failed")
                    self.assertFalse(directory.exists())
                    self.assertEqual(output.call_count, 3)
                    self.assertTrue(
                        all(
                            call.kwargs["timeout"] == 30
                            for call in output.call_args_list
                        )
                    )

    def test_chat_stream_distinguishes_reasoning_tool_output_and_terminal_close(self):
        evidence = StreamEvidence(10)
        for now, value in [
            (11, {"choices": [{"delta": {"role": "assistant"}}]}),
            (12, {"choices": [{"delta": {"reasoning_content": "private reasoning"}}]}),
            (
                13,
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "function": {
                                            "name": "dispatch",
                                            "arguments": "{}",
                                        }
                                    }
                                ]
                            }
                        }
                    ]
                },
            ),
            (14, {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            (
                15,
                {
                    "choices": [],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 20},
                },
            ),
        ]:
            evidence.feed(b"data: " + json.dumps(value).encode() + b"\n\n", now)
        evidence.feed(b"data: [DONE]\n\n", 16)
        result = evidence.summary()
        self.assertEqual(result["first_sse_seconds"], 1)
        self.assertEqual(result["first_reasoning_delta_seconds"], 2)
        self.assertEqual(result["first_output_delta_seconds"], 3)
        self.assertTrue(result["finish_event_seen"])
        self.assertTrue(result["done_marker_seen"])
        self.assertEqual(result["usage"]["completion_tokens"], 20)
        self.assertNotIn("private reasoning", json.dumps(result))

    def test_expanded_report_requires_full_schedule_and_compares_joint_success(self):
        common = dict(
            model=bench.MODEL,
            suite="expanded",
            repetition=1,
            seed=101,
            transport="compact",
            build_profile="release",
            binary_sha256="native",
            tool_calls=4,
            tool_errors=0,
            tool_seconds=1,
            model_steps=5,
            cumulative_reported_input_tokens=400,
            peak_reported_input_context_tokens=200,
            tool_text_bytes=50,
            tool_image_bytes=0,
            wire_response_bytes=100,
            private_runtime_removed=True,
            success=True,
            stop="model_finished",
            exit_code=0,
            model_tool_errors=0,
            finished_model_tool_attempts=4,
            unfinished_model_tool_attempts=0,
            receiver=dict(success=True, wrong_submissions=0, unsafe_action=False),
        )
        trials, schedule = [], []
        for task in ("entry", "inventory", "canvas"):
            for system in ("updated", "upstream"):
                success = task == "entry" or task == "inventory" and system == "updated"
                row = dict(
                    common,
                    task=task,
                    system=system,
                    workload=workload(task, 101),
                    success=success,
                    model_seconds=10 if system == "updated" else 20,
                )
                if task == "entry" and system == "updated":
                    row.update(
                        model_tool_errors=1,
                        receiver=dict(
                            success=True, wrong_submissions=1, unsafe_action=False
                        ),
                    )
                trials.append(row)
                schedule.append(
                    dict(
                        system=system,
                        task=task,
                        repetition=1,
                        seed=101,
                        timeout=90 if task == "canvas" else 180,
                    )
                )
        with tempfile.TemporaryDirectory(dir=ROOT / ".scratch") as temporary:
            directory = Path(temporary)
            (directory / "manifest.json").write_text(
                json.dumps(dict(suite="expanded", schedule=schedule))
            )
            (directory / "provenance.json").write_text(
                json.dumps(
                    dict(
                        sources={
                            system: dict(
                                binary_sha256="native",
                                transport="compact",
                                build_profile="release",
                            )
                            for system in ("updated", "upstream")
                        }
                    )
                )
            )
            path = directory / "results.json"
            path.write_text(json.dumps(trials))
            result = report.evidence_summary(directory)
            self.assertEqual(result["paired"]["matched_success_pairs"], 1)
            self.assertEqual(
                result["paired"]["outcomes"], {"both": 1, "ours_only": 1, "neither": 1}
            )
            self.assertEqual(result["paired"]["wall"]["mean_difference"], -10)
            self.assertEqual(result["systems"]["updated"]["successes"], 2)
            self.assertEqual(result["systems"]["updated"]["clean_successes"], 1)
            extra_case = directory / "1-inventory-updated"
            extra_case.mkdir()
            (extra_case / "events.jsonl").write_text(
                json.dumps(
                    {
                        "type": "tool_use",
                        "part": {
                            "tool": "skill",
                            "callID": "denied",
                            "state": {
                                "status": "error",
                                "error": "Unable to load skill",
                            },
                        },
                    }
                )
                + "\n"
            )
            audited = report.evidence_summary(directory)
            self.assertEqual(audited["systems"]["updated"]["clean_successes"], 0)
            self.assertEqual(
                audited["systems"]["updated"]["model_tool_errors"],
                result["systems"]["updated"]["model_tool_errors"] + 1,
            )
            self.assertEqual(result["systems"]["updated"]["wrong_submissions"], 1)
            self.assertIn("Conditioning on joint success", report.render(result))
            self.assertIn("not actual elapsed time", report.render(result))
            path.write_text(json.dumps(trials[:-1]))
            with self.assertRaisesRegex(ValueError, "incomplete"):
                report.evidence_summary(directory)
            trials[0]["seed"] = 102
            path.write_text(json.dumps(trials))
            with self.assertRaisesRegex(ValueError, "seed"):
                report.evidence_summary(directory)

    @classmethod
    def setUpClass(cls):
        (ROOT / ".scratch").mkdir(exist_ok=True)

    def test_cross_cohort_costs_preserve_failures_and_reject_instrumentation_changes(
        self,
    ):
        module_spec = importlib.util.spec_from_file_location(
            "expanded_compare", ROOT / "scripts/bench_expanded_compare.py"
        )
        comparison = importlib.util.module_from_spec(module_spec)
        module_spec.loader.exec_module(comparison)
        with tempfile.TemporaryDirectory(dir=ROOT / ".scratch") as temporary:
            before, after = Path(temporary) / "before", Path(temporary) / "after"
            for directory, phase in ((before, 0), (after, 1)):
                directory.mkdir()
                rows, schedule, sources = [], [], {}
                for system in ("updated", "upstream"):
                    source = dict(
                        binary_sha256=f"ours-{phase}"
                        if system == "updated"
                        else "reference",
                        transport="compact" if system == "updated" else "direct",
                        build_profile="release"
                        if system == "updated"
                        else "default Go build",
                    )
                    sources[system] = source
                    for task in ("entry", "canvas"):
                        row = dict(
                            system=system,
                            task=task,
                            repetition=1,
                            seed=101,
                            model=bench.MODEL,
                            workload={"seed": 101},
                            stop="model_finished",
                            exit_code=0,
                            success=task == "entry",
                            receiver={"wrong_submissions": 0, "unsafe_action": False},
                            model_tool_errors=0,
                            unfinished_model_tool_attempts=0,
                            finished_model_tool_attempts=1,
                            tool_calls=1,
                            tool_errors=0,
                            tool_seconds=1,
                            cumulative_reported_input_tokens=100,
                            model_seconds=(8 if phase else 20)
                            if system == "updated"
                            else 10,
                            private_runtime_removed=True,
                            **source,
                        )
                        rows.append(row)
                        schedule.append(
                            dict(
                                system=system,
                                task=task,
                                repetition=1,
                                seed=101,
                                timeout=90 if task == "canvas" else 180,
                            )
                        )
                manifest = dict(
                    suite="expanded",
                    model=bench.MODEL,
                    provider_timing=True,
                    schedule=schedule,
                    **{
                        key: "frozen"
                        for key in (
                            "harness_sha256",
                            "fixture_sha256",
                            "workload_sha256",
                            "timing_sha256",
                        )
                    },
                )
                (directory / "results.json").write_text(json.dumps(rows))
                (directory / "manifest.json").write_text(json.dumps(manifest))
                (directory / "provenance.json").write_text(
                    json.dumps({"sources": sources})
                )
            result = comparison.compare(before, after, report)
            self.assertEqual(result["trials"], 8)
            self.assertEqual(result["ours_joint_success"]["pairs"], 1)
            self.assertEqual(
                result["ours_joint_success"]["wall"]["mean_difference"], -12
            )
            self.assertEqual(result["four_way_common_success"]["pairs"], 1)
            self.assertEqual(result["paired_goal_transitions"]["neither"], 1)
            self.assertIn("Receiver-verified goals", comparison.render(result))
            manifest = json.loads((after / "manifest.json").read_text())
            manifest["fixture_sha256"] = "changed"
            (after / "manifest.json").write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "changed fixture_sha256"):
                comparison.compare(before, after, report)

    def test_model_attempts_include_host_failures_and_deduplicate_updates(self):
        def event(identity, status):
            return {
                "type": "tool_use",
                "part": {
                    "callID": identity,
                    "tool": "desktop_dispatch",
                    "state": {"status": status},
                },
            }

        summary = bench.summarize(
            [
                event("native", "running"),
                event("native", "completed"),
                event("host-rejected", "error"),
                event("in-flight", "running"),
            ],
            [],
        )
        self.assertEqual(summary["finished_model_tool_attempts"], 2)
        self.assertEqual(summary["unfinished_model_tool_attempts"], 1)
        self.assertEqual(summary["model_tool_errors"], 1)
        self.assertEqual(summary["model_tool_error_rate"], 0.5)
        self.assertEqual(summary["tool_calls"], 0)
        self.assertIsNone(summary["tool_error_rate"])

    def test_timing_partitions_unions_not_double_counted_requests(self):
        provider = [
            dict(started_monotonic=1, ended_monotonic=4, first_output_delta_seconds=1),
            dict(started_monotonic=3, ended_monotonic=7),
        ]
        journal = [dict(method="tools/call", started_monotonic=6, ended_monotonic=8)]
        result = bench.timing_summary(provider, journal, 0, 10)
        self.assertEqual(result["provider_seconds_union"], 6)
        self.assertEqual(result["native_tool_seconds_union"], 2)
        self.assertEqual(result["unattributed_seconds"], 3)
        self.assertEqual(result["first_output_delta_seconds"], [1])
        with self.assertRaisesRegex(ValueError, "Negative"):
            bench.interval_seconds([(2, 1)])

    def test_seeded_workloads_are_reproducible_and_cover_multiple_toolkits(self):
        self.assertEqual(len(WORKLOADS), 22)
        # Added families must not disturb the frozen parameters of older ones.
        self.assertEqual(workload("kwrite", 101)["params"]["text"], "naïve λ 886")
        self.assertEqual(
            {
                workload(task, 101)["app"]
                for task in ["menu", "tree", "editor", "context"]
            },
            {"Desktop Benchmark"},
        )
        self.assertEqual(workload("shipping", 101), workload("shipping", 101))
        self.assertNotEqual(
            workload("shipping", 101)["params"], workload("shipping", 102)["params"]
        )
        self.assertEqual(
            {workload(task, 101)["toolkit"] for task in WORKLOADS},
            {"GTK3", "GTK4", "Qt6", "KWrite"},
        )
        self.assertEqual(workload("drag", 101)["timeout_cap"], 90)
        with self.assertRaises(ValueError):
            workload("unknown", 101)

    def test_provider_stream_proxy_preserves_bytes_without_journaling_secrets(self):
        messages = [
            dict(type="response.reasoning_text.delta", delta="private reasoning"),
            dict(
                type="response.function_call_arguments.delta",
                delta='{"text":"café SECRET_SENTINEL"}',
            ),
            dict(
                type="response.completed",
                response={
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 20,
                        "metadata": "SECRET_SENTINEL",
                    }
                },
            ),
        ]
        payload = (
            b"".join(
                b"data: " + json.dumps(message, ensure_ascii=False).encode() + b"\n\n"
                for message in messages
            )
            + b"data: [DONE]\n\n"
        )

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.server.request_path = self.path
                self.server.authorization = self.headers.get("Authorization")
                self.server.body = self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for chunk in (payload[:23], payload[23:111], payload[111:]):
                    self.wfile.write(chunk)
                    self.wfile.flush()

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(dir=ROOT / ".scratch") as temporary:
                journal = Path(temporary) / "provider.jsonl"
                with ProviderTimingProxy(
                    f"http://127.0.0.1:{server.server_port}/base", journal
                ) as proxy:
                    request = Request(
                        proxy.url + "/responses?x=1",
                        data=b'{"prompt":"SECRET_SENTINEL"}',
                        headers={"Authorization": "Bearer SECRET_SENTINEL"},
                    )
                    with urlopen(request, timeout=10) as response:
                        self.assertEqual(response.read(), payload)
                self.assertEqual(server.request_path, "/base/responses?x=1")
                self.assertEqual(server.authorization, "Bearer SECRET_SENTINEL")
                record = json.loads(journal.read_text())
                self.assertTrue(record["completed"])
                self.assertEqual(record["response_bytes"], len(payload))
                self.assertEqual(record["event_counts"]["response.completed"], 1)
                self.assertEqual(
                    record["usage"], {"input_tokens": 10, "output_tokens": 20}
                )
                self.assertIsNotNone(record["first_output_delta_seconds"])
                self.assertNotIn("SECRET_SENTINEL", journal.read_text())
                self.assertNotIn("private reasoning", journal.read_text())
            evidence = StreamEvidence(10)
            evidence.feed(payload[:111], 11)
            evidence.feed(payload[111:], 12)
            self.assertEqual(evidence.summary()["first_reasoning_delta_seconds"], 1)
            self.assertEqual(evidence.summary()["first_output_delta_seconds"], 2)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_report_keeps_failed_vision_costs_separate_and_rejects_mixed_snapshots(
        self,
    ):
        common = dict(
            system="baseline",
            repetition=1,
            model=bench.MODEL,
            model_seconds=20,
            tool_calls=4,
            tool_errors=1,
            tool_seconds=2,
            cumulative_reported_input_tokens=400,
            peak_reported_input_context_tokens=200,
            tool_text_bytes=50,
            tool_image_bytes=0,
            wire_response_bytes=100,
            private_runtime_removed=True,
            binary_sha256="a",
            success=True,
            stop="model_finished",
        )
        trials = [
            dict(common, task="easy"),
            dict(common, task="visual", success=False, model_seconds=1),
        ]
        with tempfile.TemporaryDirectory(
            dir=ROOT / ".scratch", prefix="report-test-"
        ) as directory:
            directory = Path(directory)
            path = directory / "results.json"
            path.write_text(json.dumps(trials))
            result = report.evidence_summary(directory)
            system = result["systems"]["baseline"]
            self.assertEqual(system["semantic"]["mean_model_seconds"], 20)
            self.assertEqual(system["all"]["mean_model_seconds"], 10.5)
            self.assertEqual(system["all"]["tool_error_rate"], 0.25)
            self.assertEqual(system["all"]["successes"], 1)
            self.assertTrue(system["all_private_runtimes_removed"])
            self.assertIn("unequal trial counts", report.render(result))
            self.assertIn("unavailable logs", report.render(result))
            self.assertIn("20.0s · 0.4k", report.render(result))
            self.assertNotIn("1.0s · 0.4k", report.render(result))
            for trial in trials:
                case = directory / f"1-{trial['task']}-baseline"
                case.mkdir()
                parts = [
                    {
                        "type": "tool_use",
                        "part": {
                            "callID": identity,
                            "tool": "desktop_dispatch",
                            "state": {"status": status},
                        },
                    }
                    for identity, status in [
                        ("ok", "running"),
                        ("ok", "completed"),
                        ("host-rejection", "error"),
                    ]
                ]
                (case / "events.jsonl").write_text(
                    "".join(json.dumps(part) + "\n" for part in parts)
                )
            measured = report.evidence_summary(directory)["systems"]["baseline"][
                "model_tools"
            ]
            self.assertEqual(
                measured,
                {"instrumented_trials": 2, "finished": 4, "unfinished": 0, "errors": 2},
            )
            provenance = directory / "provenance.json"
            provenance.write_text(
                json.dumps({"sources": {"baseline": {"binary_sha256": "b"}}})
            )
            with self.assertRaisesRegex(ValueError, "differs from provenance"):
                report.evidence_summary(directory)
            provenance.write_text(
                json.dumps({"sources": {"baseline": {"binary_sha256": "a"}}})
            )
            self.assertIn("provenance", report.evidence_summary(directory))
            trials[1]["binary_sha256"] = "b"
            path.write_text(json.dumps(trials))
            with self.assertRaisesRegex(ValueError, "Mixed executable"):
                report.evidence_summary(directory)
            trials[1]["binary_sha256"] = "a"
            trials[1]["transport"] = "compact"
            path.write_text(json.dumps(trials))
            with self.assertRaisesRegex(ValueError, "Mixed MCP transports"):
                report.evidence_summary(directory)
            trials[1]["transport"] = "direct"
            trials[1]["build_profile"] = "release"
            path.write_text(json.dumps(trials))
            with self.assertRaisesRegex(ValueError, "Mixed build profiles"):
                report.evidence_summary(directory)
            trials[0]["build_profile"] = "release"
            path.write_text(json.dumps(trials))
            rendered = report.render(report.evidence_summary(directory))
            self.assertIn(f"{report.LABELS['baseline']}: release", rendered)
            self.assertNotIn("Rust debug builds", rendered)

    def test_cache_context_errors_and_non_tool_catalog_are_separate(self):
        events = [
            {
                "type": "step_finish",
                "part": {
                    "tokens": {
                        "input": 10,
                        "output": 3,
                        "reasoning": 2,
                        "cache": {"read": 20, "write": 5},
                    }
                },
            },
            {"type": "step_finish", "part": {"tokens": {"input": 40}}},
            {"type": "error", "error": {"code": "provider.auth"}},
        ]
        common = dict(
            seconds=0.25, text_bytes=7, image_bytes=11, response_bytes=100, error=False
        )
        journal = [
            dict(common, method="tools/list"),
            dict(common, method="tools/call"),
            dict(common, method="tools/call", error=True),
        ]
        result = bench.summarize(events, journal)
        self.assertEqual(result["tool_calls"], 2)
        self.assertEqual(result["tool_error_rate"], 0.5)
        self.assertEqual(result["tool_seconds"], 0.5)
        self.assertEqual(result["catalog_bytes"], 100)
        self.assertEqual(result["wire_response_bytes"], 200)
        self.assertEqual(result["tool_text_bytes"], 14)
        self.assertEqual(result["tool_image_bytes"], 22)
        self.assertEqual(result["peak_reported_input_context_tokens"], 40)
        self.assertEqual(result["cumulative_reported_input_tokens"], 75)
        self.assertEqual(result["input_tokens"], 50)
        self.assertEqual(result["model_errors"], [{"code": "provider.auth"}])

    def test_proxy_preserves_native_bytes_and_counts_actual_responses(self):
        scratch = ROOT / ".scratch"
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(
            dir=scratch, prefix="proxy-test-"
        ) as directory:
            journal = Path(directory) / "rpc.jsonl"
            source = """
import json, sys
for line in sys.stdin.buffer:
    request = json.loads(line)
    if 'id' not in request: continue
    result = {'content': [{'type': 'text', 'text': 'café'},
                         {'type': 'image', 'data': 'aW1hZ2U=', 'mimeType': 'image/png'}],
              'isError': request['id'] == 2}
    response = {'jsonrpc': '2.0', 'id': request['id'], 'result': result}
    sys.stdout.buffer.write((json.dumps(response) + '\\n').encode())
    sys.stdout.buffer.flush()
"""
            requests = [
                dict(
                    jsonrpc="2.0",
                    id=i,
                    method="tools/call",
                    params={"name": "observe", "arguments": {}},
                )
                for i in (1, 2)
            ]
            completed = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/bench-mcp-proxy.py"),
                    "--journal",
                    str(journal),
                    "--",
                    sys.executable,
                    "-u",
                    "-c",
                    source,
                ],
                input=b"".join((json.dumps(row) + "\n").encode() for row in requests),
                capture_output=True,
                timeout=15,
                check=True,
            )
            responses = [json.loads(line) for line in completed.stdout.splitlines()]
            self.assertEqual([row["id"] for row in responses], [1, 2])
            records = bench.rows(journal)
            usage = json.loads(journal.with_suffix(".usage.json").read_text())
            self.assertGreaterEqual(usage["user_seconds"], 0)
            self.assertGreaterEqual(usage["system_seconds"], 0)
            self.assertGreater(usage["elapsed_seconds"], 0)
            self.assertGreater(usage["max_rss_kib"], 0)
            self.assertEqual([row["error"] for row in records], [False, True])
            self.assertEqual([row["text_bytes"] for row in records], [5, 5])
            self.assertEqual([row["image_bytes"] for row in records], [5, 5])
            self.assertEqual(
                sum(row["response_bytes"] for row in records),
                len(completed.stdout) - len(responses),
            )
            self.assertEqual(
                base64.b64decode(responses[0]["result"]["content"][1]["data"]), b"image"
            )


if __name__ == "__main__":
    unittest.main()
