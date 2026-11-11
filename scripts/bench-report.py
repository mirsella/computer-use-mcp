#!/usr/bin/env python3
"""Summarize a completed desktop pilot without mixing failed vision costs in.

Consumes only synthetic benchmark evidence. Never reads provider configuration.
"""

import argparse
import html
import json
import re
import statistics
from collections import Counter
from pathlib import Path

SYSTEMS = ("baseline", "updated", "upstream")
MODEL = "opencode-go/deepseek-v4.1-flash#max"
LABELS = {
    "baseline": "Ours · baseline",
    "updated": "Ours · updated",
    "upstream": "Open Computer Use",
}
TASKS = ("easy", "medium", "hard", "visual")


def aggregate(trials):
    if not trials:
        return {"attempts": 0, "successes": 0}
    required = (
        "model_seconds",
        "tool_calls",
        "tool_errors",
        "tool_seconds",
        "cumulative_reported_input_tokens",
        "peak_reported_input_context_tokens",
        "tool_text_bytes",
        "tool_image_bytes",
        "wire_response_bytes",
    )
    complete = [trial for trial in trials if all(key in trial for key in required)]
    result = {
        "attempts": len(trials),
        "successes": sum(bool(trial.get("success")) for trial in trials),
        "instrumented_attempts": len(complete),
        "model_or_setup_errors": sum(
            bool(trial.get("model_errors"))
            or trial.get("stop") in ("harness_failure", "model_exit_error")
            for trial in trials
        ),
        "deadlines": sum(trial.get("stop") == "deadline" for trial in trials),
    }
    if not complete:
        return result
    for key in required:
        values = [trial[key] for trial in complete]
        result[f"sum_{key}"] = sum(values)
        result[f"mean_{key}"] = statistics.mean(values)
        result[f"min_{key}"] = min(values)
        result[f"max_{key}"] = max(values)
    result["tool_error_rate"] = (
        result["sum_tool_errors"] / result["sum_tool_calls"]
        if result["sum_tool_calls"]
        else None
    )
    return result


def evidence_summary(directory):
    manifest = directory / "manifest.json"
    if (
        manifest.exists()
        and json.loads(manifest.read_text()).get("suite") == "expanded"
    ):
        from bench_expanded_report import evidence_summary as expanded_summary

        return expanded_summary(directory, aggregate)
    trials = json.loads((directory / "results.json").read_text())
    if not trials:
        raise ValueError("No benchmark results")
    if any(
        trial["system"] not in SYSTEMS or trial["task"] not in TASKS for trial in trials
    ):
        raise ValueError("Unknown benchmark system or task")
    if any(
        trial.get("model") != MODEL and trial.get("stop") != "harness_failure"
        for trial in trials
    ):
        raise ValueError("Results must use the requested model and variant")
    if len(
        {(trial["system"], trial["task"], trial["repetition"]) for trial in trials}
    ) != len(trials):
        raise ValueError("Duplicate trial identities")
    report = {
        "model": MODEL,
        "trials": len(trials),
        "systems": {},
    }
    provenance = directory / "provenance.json"
    if provenance.exists():
        report["provenance"] = json.loads(provenance.read_text())
    for system in SYSTEMS:
        selected = [trial for trial in trials if trial["system"] == system]
        if not selected:
            continue
        transports = {trial.get("transport", "direct") for trial in selected}
        if len(transports) > 1:
            raise ValueError(f"Mixed MCP transports for {system}")
        profiles = {
            trial.get(
                "build_profile", "default Go" if system == "upstream" else "debug"
            )
            for trial in selected
        }
        if len(profiles) > 1:
            raise ValueError(f"Mixed build profiles for {system}")
        if (
            len(
                {
                    trial["binary_sha256"]
                    for trial in selected
                    if "binary_sha256" in trial
                }
            )
            > 1
        ):
            raise ValueError(f"Mixed executable snapshots for {system}")
        codes, outcomes, model_tools = Counter(), Counter(), Counter()
        for trial in selected:
            path = (
                directory
                / f"{trial['repetition']}-{trial['task']}-{system}"
                / "events.jsonl"
            )
            if not path.exists():
                continue
            parts = {}
            for index, line in enumerate(path.read_text().splitlines()):
                event = json.loads(line)
                part = event.get("part", {})
                if event.get("type") != "tool_use" or not part.get(
                    "tool", ""
                ).startswith("desktop_"):
                    continue
                parts[part.get("callID", part.get("id", index))] = part
            model_tools["instrumented_trials"] += 1
            for part in parts.values():
                state = part.get("state", {})
                if state.get("status") not in ("completed", "error"):
                    model_tools["unfinished"] += 1
                    continue
                model_tools["finished"] += 1
                if state["status"] != "error":
                    continue
                model_tools["errors"] += 1
                message = str(state.get("error", ""))
                code = re.search(r"(?m)^Code:\s*(\w+)", message)
                outcome = re.search(r"(?m)^Outcome:\s*(\w+)", message)
                codes[code[1] if code else "unclassified"] += 1
                outcomes[outcome[1] if outcome else "unclassified"] += 1
        report["systems"][system] = {
            "all": aggregate(selected),
            "transport": next(iter(transports)),
            "build_profile": next(iter(profiles)),
            "model_tools": {
                key: model_tools[key]
                for key in ("instrumented_trials", "finished", "unfinished", "errors")
            },
            "semantic": aggregate(
                [trial for trial in selected if trial["task"] != "visual"]
            ),
            "tasks": {
                task: aggregate([trial for trial in selected if trial["task"] == task])
                for task in TASKS
            },
            "binary_sha256": sorted(
                {
                    trial["binary_sha256"]
                    for trial in selected
                    if "binary_sha256" in trial
                }
            ),
            "all_private_runtimes_removed": all(
                trial.get("private_runtime_removed", False) for trial in selected
            ),
            "model_tool_error_codes": dict(codes),
            "model_tool_error_outcomes": dict(outcomes),
        }
        source = report.get("provenance", {}).get("sources", {}).get(system)
        if source and report["systems"][system]["binary_sha256"] != [
            source["binary_sha256"]
        ]:
            raise ValueError(f"Executable differs from provenance for {system}")
    return report


def render(report):
    if report.get("suite") == "expanded":
        from bench_expanded_report import render as expanded_render

        return expanded_render(report)
    systems = report["systems"]
    counts = {
        task["attempts"]
        for system in systems.values()
        for task in system["tasks"].values()
    }
    repeats = (
        f"{next(iter(counts))} runs per task/system"
        if len(counts) == 1
        else "unequal trial counts"
    )

    def chart(title, metric, format_value, subtitle):
        maximum = max(
            (system["semantic"].get(metric, 0) for system in systems.values()),
            default=0,
        )
        rows = []
        for key, system in systems.items():
            value = system["semantic"].get(metric)
            if value is None:
                continue
            width = 100 * value / maximum if maximum else 0
            rows.append(
                f'<div class="bar-row"><span>{LABELS[key]}</span><div class="track"><div class="bar {key}" style="width:{width:.2f}%"></div></div><b>{format_value(value)}</b></div>'
            )
        return f'<section><h2>{title}</h2><p class="caption">{subtitle}</p>{"".join(rows)}</section>'

    charts = "".join(
        (
            chart(
                "Model-run wall time",
                "mean_model_seconds",
                lambda value: f"{value:.1f}s",
                "Mean per semantic task · lower is better",
            ),
            chart(
                "Native desktop tool calls",
                "mean_tool_calls",
                lambda value: f"{value:.1f}",
                "Mean per semantic task · calls reaching the native MCP server",
            ),
            chart(
                "Native MCP time",
                "mean_tool_seconds",
                lambda value: f"{value:.2f}s",
                "Mean summed RPC duration per semantic task · not CPU time",
            ),
            chart(
                "Cumulative reported input",
                "mean_cumulative_reported_input_tokens",
                lambda value: f"{value / 1000:.1f}k",
                "Mean per semantic task · includes repeated context",
            ),
        )
    )
    headers = "".join(f"<th>{LABELS[key]}</th>" for key in systems)
    body = []
    names = {
        "easy": "Entry + submit",
        "medium": "2,000-row list + footer",
        "hard": "Unicode form + modal",
        "visual": "Pixel-only canvas + typing",
    }
    for task in TASKS:
        cells = []
        for system in systems.values():
            result = system["tasks"][task]
            cls = "ok" if result["successes"] == result["attempts"] else "fail"
            cells.append(
                f'<td class="{cls}">{result["successes"]}/{result["attempts"]}</td>'
            )
        body.append(f"<tr><td>{names[task]}</td>{''.join(cells)}</tr>")
    cost_rows = []
    for task in TASKS:
        if task == "visual":
            continue
        cells = []
        for system in systems.values():
            result = system["tasks"][task]
            seconds = result.get("mean_model_seconds")
            tokens = result.get("mean_cumulative_reported_input_tokens")
            value = (
                f"{seconds:.1f}s · {tokens / 1000:.1f}k"
                if seconds is not None and tokens is not None
                else "—"
            )
            cells.append(f"<td>{value}</td>")
        cost_rows.append(f"<tr><td>{names[task]}</td>{''.join(cells)}</tr>")
    errors = " · ".join(
        f"{LABELS[key]}: {system['all'].get('sum_tool_errors', 0)}/{system['all'].get('sum_tool_calls', 0)}"
        for key, system in systems.items()
    )
    model_errors = " · ".join(
        f"{LABELS[key]}: {system['model_tools']['errors']}/{system['model_tools']['finished']} ({system['model_tools']['unfinished']} unfinished)"
        if system["model_tools"]["instrumented_trials"] == system["all"]["attempts"]
        else f"{LABELS[key]}: unavailable logs"
        for key, system in systems.items()
    )
    surfaces = " · ".join(
        f"{LABELS[key]}: {system['transport']}" for key, system in systems.items()
    )
    builds = " · ".join(
        f"{LABELS[key]}: {system['build_profile']}" for key, system in systems.items()
    )
    capability_notes = []
    for key, system in systems.items():
        visual = system["tasks"]["visual"]
        if visual["attempts"] and visual.get("sum_tool_image_bytes") == 0:
            capability_notes.append(
                f"{LABELS[key]} returned no image bytes across {visual['attempts']} canvas trials."
            )
        if system["all"]["deadlines"]:
            capability_notes.append(
                f"{LABELS[key]} reached {system['all']['deadlines']} trial deadline(s)."
            )
    capability_note = html.escape(" ".join(capability_notes))
    teardown = all(
        system["all_private_runtimes_removed"] for system in systems.values()
    )
    return f"""<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Desktop benchmark</title><style>
:root{{color-scheme:light dark}}*{{box-sizing:border-box}}body{{margin:0;color:var(--foreground,CanvasText);font:14px var(--font-sans,system-ui,sans-serif)}}
p{{line-height:1.55;margin:0}}.intro{{color:var(--muted-foreground,#8e98aa);font-size:12px}}section{{margin-top:23px}}h2{{font-size:14px;font-weight:600;margin:0 0 4px}}.caption{{font-size:11px;color:var(--muted-foreground,#8e98aa);margin-bottom:10px}}
.bar-row{{display:grid;grid-template-columns:145px minmax(30px,1fr) 60px;gap:10px;align-items:center;margin:9px 0;font-size:12px}}.bar-row b{{font:12px var(--font-mono,monospace);text-align:right}}.track{{height:12px;background:color-mix(in srgb,currentColor 7%,transparent);border-radius:2px}}.bar{{height:100%;border-radius:2px}}.baseline{{background:var(--muted-foreground,#8991a4)}}.updated{{background:var(--chart-2,#31bca4)}}.upstream{{background:var(--chart-4,#e5a44f)}}
table{{width:100%;border-collapse:collapse;margin-top:12px;font-size:12px}}th,td{{padding:9px 5px;border-bottom:1px solid color-mix(in srgb,currentColor 14%,transparent);text-align:right}}th:first-child,td:first-child{{text-align:left;padding-left:0}}th{{font-size:11px;font-weight:500;color:var(--muted-foreground,#8e98aa)}}td:not(:first-child){{font-family:var(--font-mono,monospace)}}.ok{{color:var(--success,#31bca4)}}.fail{{color:var(--warning,#e5a44f)}}.notes{{font-size:11px;color:var(--muted-foreground,#8e98aa);line-height:1.55;margin-top:18px}}.notes p+p{{margin-top:8px}}
</style></head><body>
<p class="intro">{report["trials"]} trials · {repeats} · {html.escape(report["model"])}<br>{html.escape(surfaces)}</p>
{charts}
<section><h2>Semantic tasks · mean time / cumulative input</h2><table><thead><tr><th>Task</th>{headers}</tr></thead><tbody>{"".join(cost_rows)}</tbody></table></section>
<section><h2>Independent receiver success</h2><table><thead><tr><th>Task</th>{headers}</tr></thead><tbody>{"".join(body)}</tbody></table></section>
<div class="notes"><p>Model tool errors / finished attempts, including host schema rejections: {model_errors}.</p>
<p>Native tool errors / calls, all tasks: {errors}. Errors include completed dispatches whose post-action observation failed; they are not all failed input delivery. Host argument-validation failures are outside native RPC counts.</p>
<p>{capability_note}</p>
<p>Cost charts exclude the pixel-only task, so unavailable screenshots do not look artificially cheap. Context is provider input + cache reads/writes, summed per model step, not unique tokens. Model-run time includes tools. Screenshot escalation was allowed equally; the model chose whether to use it.</p>
<p>Small synthetic pilot, not a significance test. Declared builds: {html.escape(builds)}; result schemas and verification semantics differ. All private runtime directories removed: {str(teardown).lower()}. No physical desktop interaction.</p></div>
</body></html>"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    report = evidence_summary(args.directory)
    (args.directory / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    (args.directory / "report.html").write_text(render(report))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
