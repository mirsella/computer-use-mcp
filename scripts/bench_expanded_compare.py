#!/usr/bin/env python3
"""Compare complete frozen expanded cohorts, retaining failures and seed coverage."""

import argparse
import html
import importlib.util
import json
from pathlib import Path

from bench_expanded_report import extended_aggregate, paired_stats, WORKLOADS


def compare(before_directory, after_directory, base_report):
    reports = [
        base_report.evidence_summary(path)
        for path in (before_directory, after_directory)
    ]
    if any(report.get("suite") != "expanded" for report in reports):
        raise ValueError("Both inputs must be complete expanded cohorts")
    manifests = [report["manifest"] for report in reports]
    for field in (
        "model",
        "provider_timing",
        "schedule",
        "harness_sha256",
        "fixture_sha256",
        "workload_sha256",
        "timing_sha256",
    ):
        if manifests[0].get(field) != manifests[1].get(field):
            raise ValueError(f"Expanded comparison changed {field}")
    if (
        reports[0]["provenance"]["sources"]["upstream"]["binary_sha256"]
        != reports[1]["provenance"]["sources"]["upstream"]["binary_sha256"]
    ):
        raise ValueError("Reference executable changed between cohorts")
    rows = [
        json.loads((directory / "results.json").read_text())
        for directory in (before_directory, after_directory)
    ]
    indices = [
        {(row["system"], row["task"], row["repetition"]): row for row in cohort}
        for cohort in rows
    ]
    for identity, before in indices[0].items():
        after = indices[1][identity]
        if (
            before.get("workload")
            and after.get("workload")
            and before["workload"] != after["workload"]
        ):
            raise ValueError("Seeded workload changed between cohorts")
    own_pairs = [
        (indices[1][identity], row)
        for identity, row in indices[0].items()
        if identity[0] == "updated"
    ]
    joint = [
        (a, b)
        for a, b in own_pairs
        if a.get("success") and b.get("success") and WORKLOADS[a["task"]][1] != "visual"
    ]
    four_way = []
    for identity, row in indices[0].items():
        if identity[0] != "updated" or WORKLOADS[row["task"]][1] == "visual":
            continue
        reference = ("upstream", identity[1], identity[2])
        values = [
            row,
            indices[1][identity],
            indices[0][reference],
            indices[1][reference],
        ]
        if all(value.get("success") for value in values):
            four_way.append(values)
    labels = ("ours_before", "ours_after", "reference_before", "reference_after")
    result = dict(
        model=reports[0]["model"],
        trials=sum(report["trials"] for report in reports),
        before=reports[0],
        after=reports[1],
        ours_joint_success=dict(
            pairs=len(joint),
            tasks=sorted({a["task"] for a, _ in joint}),
            wall=paired_stats(joint, "model_seconds"),
            context=paired_stats(joint, "cumulative_reported_input_tokens"),
            native=paired_stats(joint, "tool_seconds"),
        ),
        four_way_common_success=dict(
            pairs=len(four_way),
            tasks=sorted({group[0]["task"] for group in four_way}),
            groups={
                label: extended_aggregate(
                    [group[index] for group in four_way], base_report.aggregate
                )
                for index, label in enumerate(labels)
            },
        ),
    )
    result["ours_joint_success"]["meaning"] = (
        "paired_stats uses ours=after, upstream=before here; not a reference-system comparison"
    )
    result["paired_goal_transitions"] = dict(
        improved=sum(
            not b.get("success") and bool(a.get("success")) for a, b in own_pairs
        ),
        regressed=sum(
            bool(b.get("success")) and not a.get("success") for a, b in own_pairs
        ),
        both=sum(
            bool(a.get("success")) and bool(b.get("success")) for a, b in own_pairs
        ),
        neither=sum(
            not a.get("success") and not b.get("success") for a, b in own_pairs
        ),
    )
    return result


def render(report):
    before, after = report["before"], report["after"]
    series = [
        (before["systems"]["updated"], "Ours · before"),
        (after["systems"]["updated"], "Ours · optimized"),
        (after["systems"]["upstream"], "Open Computer Use · retest"),
    ]
    total = sum(data["attempts"] for data, _ in series[:1])
    colors = [
        "var(--muted-foreground,#888)",
        "var(--chart-2,#2aa88a)",
        "var(--chart-3,#8b74c8)",
    ]

    def chart(title, values, suffix, note):
        maximum = max(values, default=0) or 1
        bars = "".join(
            f'<div class="barrow"><span>{html.escape(label)}</span><div class="track"><i style="width:{value / maximum * 100:.2f}%;background:{color}"></i></div><b>{value:,.1f}{suffix}</b></div>'
            for (_, label), value, color in zip(series, values, colors)
        )
        return f'<section><h3>{title}</h3><div class="chart">{bars}</div><small>{note}</small></section>'

    charts = chart(
        "Receiver-verified goals",
        [data["successes"] for data, _ in series],
        f" / {total}",
        "All attempts, including unsupported capabilities and deadlines.",
    )
    charts += chart(
        "Clean completions",
        [data["clean_successes"] for data, _ in series],
        f" / {total}",
        "Goal achieved with no tool error, wrong submission, unsafe action, deadline or unfinished call.",
    )
    groups = report["four_way_common_success"]["groups"]
    if report["four_way_common_success"]["pairs"]:
        selected = [
            groups[key] for key in ("ours_before", "ours_after", "reference_after")
        ]
        charts += chart(
            "Matched semantic time",
            [data["model_seconds"]["mean"] for data in selected],
            "s",
            f"{report['four_way_common_success']['pairs']} identical seed/task pairs succeeded in all four runs; failed capabilities excluded from this cost view only.",
        )
        charts += chart(
            "Matched cumulative input",
            [
                data["cumulative_reported_input_tokens"]["mean"] / 1000
                for data in selected
            ],
            "k",
            "Provider-reported repeated input plus cache reads/writes, not unique tokens.",
        )
    table = "".join(
        f"<tr><td>{html.escape(WORKLOADS[task][2])}</td>"
        + "".join(
            f"<td>{data['tasks'][task]['successes']}/{data['tasks'][task]['attempts']} · {data['tasks'][task]['clean_successes']} clean</td>"
            for data, _ in series
        )
        + "</tr>"
        for task in after["task_order"]
    )
    headers = "".join(f"<th>{html.escape(label)}</th>" for _, label in series)
    interruptions = sum(
        phase.get("archived_incomplete_attempts", 0) for phase in (before, after)
    )
    interruption_note = (
        f" {interruptions} incomplete attempt(s) from driver interruption were archived before resuming fresh private sessions; completed trials were preserved. The resumed phase spans an execution break."
        if interruptions
        else ""
    )
    variant_count = len({row["seed"] for row in after["manifest"]["schedule"]})
    counters = "; ".join(
        f"{label}: {data['model_tool_errors']}/{data['finished_model_tool_attempts']} tool errors, {data['wrong_submissions']} wrong submissions"
        for data, label in series
    )
    phases = []
    for data, label in series:
        timing = data["timing"]

        def sec(field):
            value = timing[field]["mean"]
            return "—" if value is None else f"{value:.2f}s"

        phases.append(
            f"<tr><td>{html.escape(label)}</td><td>{sec('provider_seconds_union')}</td><td>{sec('native_tool_seconds_union')}</td><td>{sec('unattributed_seconds')}</td><td>{timing['complete']}/{timing['instrumented']}</td></tr>"
        )
    return f"""<!doctype html><html><head><meta charset="utf-8"><style>
body{{margin:0;color:var(--foreground,CanvasText);font:14px/1.5 var(--font-sans,system-ui)}}h3{{font-size:15px;margin:0 0 8px}}section{{margin:22px 0}}p,small{{color:var(--muted-foreground,#888)}}.chart{{height:116px}}.barrow{{display:grid;grid-template-columns:minmax(115px,1.3fr) minmax(60px,2fr) 88px;align-items:center;gap:10px;height:36px}}.track{{height:13px;background:var(--secondary,rgba(128,128,128,.12));border-radius:3px}}i{{height:13px;display:block;border-radius:3px}}b{{text-align:right;font:13px var(--font-mono,monospace)}}table{{width:100%;border-collapse:collapse;font-size:12px}}td,th{{text-align:left;padding:7px 4px;border-bottom:1px solid var(--border,#8884)}}th{{color:var(--muted-foreground,#888);font-weight:500}}.scroll{{overflow-x:auto}}@media(max-width:500px){{.barrow{{grid-template-columns:118px 1fr 72px;font-size:11px;gap:6px}}}}
</style></head><body><p>{report["trials"]} scheduled trials · {len(after["task_order"])} families · {variant_count} seeded variants · DeepSeek V4.1 Flash max · frozen binaries</p>{charts}<section><h3>Goals / attempts · clean completions</h3><div class="scroll"><table><thead><tr><th>Workload</th>{headers}</tr></thead><tbody>{table}</tbody></table></div></section><section><h3>Instrumented phases · all attempts</h3><div class="scroll"><table><tr><th>Implementation</th><th>Provider HTTP</th><th>Native MCP</th><th>Unattributed</th><th>Capture coverage</th></tr>{"".join(phases)}</table></div><small>Means of interval unions; provider requests include network, queueing and streaming, not pure model compute. Columns can overlap.</small></section><p>{html.escape(counters)}.</p><p>Matched costs condition on four-way success and explicitly omit unsuccessful capabilities. Full reports retain every trial, per-request first reasoning/output deltas, distributions, wrong submissions and failures. Before/after phases ran at different times; the unchanged reference was rerun, but this is not a causal estimate of provider speed.{interruption_note} Bootstrap ranges describe the fixed workload mix, not all applications. All interaction used owned private Wayland sessions; teardown status is recorded per trial.</p></body></html>"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location(
        "bench_report", Path(__file__).with_name("bench-report.py")
    )
    base = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(base)
    report = compare(args.before, args.after, base)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "comparison.json").write_text(json.dumps(report, indent=2))
    (args.output / "comparison.html").write_text(render(report))
    print(
        json.dumps(
            {
                "trials": report["trials"],
                "ours_joint_success": report["ours_joint_success"],
                "goal_transitions": report["paired_goal_transitions"],
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
