"""Expanded receiver-verified, paired benchmark analysis; no credential access."""

from collections import Counter
import html
import json
import random
import re
import statistics

from desktop_bench_workloads import WORKLOADS

MODEL = "opencode-go/deepseek-v4.1-flash#max"
LABELS = {"updated": "Ours", "upstream": "Open Computer Use"}


def percentile(values, fraction):
    if not values:
        return None
    values = sorted(values)
    position = (len(values) - 1) * fraction
    left = int(position)
    return values[left] + (values[min(left + 1, len(values) - 1)] - values[left]) * (
        position - left
    )


def stats(values):
    values = [value for value in values if value is not None]
    if not values:
        return {"n": 0, "mean": None, "median": None, "p90": None}
    return dict(
        n=len(values),
        mean=statistics.mean(values),
        median=statistics.median(values),
        p90=percentile(values, 0.9),
    )


def clean(trial):
    receiver = trial.get("receiver")
    return bool(
        trial.get("success")
        and receiver is not None
        and not receiver.get("unsafe_action")
        and receiver.get("wrong_submissions") == 0
        and trial.get("model_tool_errors") == 0
        and trial.get("unfinished_model_tool_attempts") == 0
        and not trial.get("model_errors")
        and not trial.get("other_tool_attempts")
        and trial.get("stop") == "model_finished"
        and trial.get("exit_code") == 0
    )


def extended_aggregate(trials, aggregate):
    result = aggregate(trials)
    result.update(
        clean_successes=sum(clean(trial) for trial in trials),
        receiver_instrumented=sum("receiver" in trial for trial in trials),
        wrong_submissions=sum(
            trial.get("receiver", {}).get("wrong_submissions", 0) for trial in trials
        ),
        unsafe_actions=sum(
            bool(trial.get("receiver", {}).get("unsafe_action")) for trial in trials
        ),
        model_tool_errors=sum(trial.get("model_tool_errors", 0) for trial in trials),
        finished_model_tool_attempts=sum(
            trial.get("finished_model_tool_attempts", 0) for trial in trials
        ),
        unfinished_model_tool_attempts=sum(
            trial.get("unfinished_model_tool_attempts", 0) for trial in trials
        ),
    )
    for key in (
        "model_seconds",
        "tool_seconds",
        "output_tokens",
        "reasoning_tokens",
        "model_steps",
        "cumulative_reported_input_tokens",
        "peak_reported_input_context_tokens",
        "setup_seconds",
        "fixture_startup_seconds",
    ):
        result[key] = stats([trial.get(key) for trial in trials])
    result["other_model_tool_attempts"] = sum(
        trial.get("other_tool_attempts", 0) for trial in trials
    )
    timings = [trial["timing"] for trial in trials if "timing" in trial]
    result["timing"] = {
        key: stats([row.get(key) for row in timings])
        for key in (
            "provider_seconds_union",
            "native_tool_seconds_union",
            "unattributed_seconds",
            "start_to_first_provider_seconds",
        )
    }
    for key in (
        "first_body_seconds",
        "first_sse_seconds",
        "first_output_delta_seconds",
        "first_reasoning_delta_seconds",
    ):
        result["timing"][key] = stats(
            [value for row in timings for value in row.get(key, [])]
        )
    result["timing"].update(
        instrumented=len(timings),
        complete=sum(bool(row.get("capture_complete")) for row in timings),
        stream_requests=sum(row.get("provider_stream_requests", 0) for row in timings),
        transport_errors=sum(
            row.get("provider_transport_errors", 0) for row in timings
        ),
        inspection_errors=sum(
            row.get("stream_inspection_errors", 0) for row in timings
        ),
    )
    return result


def paired_stats(pairs, key):
    selected = [(a[key], b[key], a["task"]) for a, b in pairs if key in a and key in b]
    if not selected:
        return {"pairs": 0}
    differences = [a - b for a, b, _ in selected]
    by_task = {}
    for a, b, task in selected:
        by_task.setdefault(task, []).append(a - b)
    rng = random.Random(1904)
    resamples = []
    for _ in range(3000):
        sample = [
            rng.choice(values)
            for values in by_task.values()
            for _ in range(len(values))
        ]
        resamples.append(statistics.mean(sample))
    return dict(
        pairs=len(selected),
        ours_mean=statistics.mean(a for a, _, _ in selected),
        upstream_mean=statistics.mean(b for _, b, _ in selected),
        mean_difference=statistics.mean(differences),
        median_difference=statistics.median(differences),
        ours_faster=sum(value < 0 for value in differences),
        descriptive_stratified_bootstrap_95=[
            percentile(resamples, 0.025),
            percentile(resamples, 0.975),
        ],
    )


def error_classification(path):
    if not path.exists():
        return {"available": False}
    parts, codes, outcomes = {}, Counter(), Counter()
    for index, line in enumerate(path.read_text().splitlines()):
        event = json.loads(line)
        part = event.get("part", {})
        if event.get("type") == "tool_use":
            parts[part.get("callID", part.get("id", index))] = part
    for part in parts.values():
        state = part.get("state", {})
        if state.get("status") != "error":
            continue
        message = str(state.get("error", ""))
        code = re.search(r"(?m)^Code:\s*(\w+)", message)
        outcome = re.search(r"(?m)^Outcome:\s*(\w+)", message)
        codes[code[1] if code else "unclassified"] += 1
        outcomes[outcome[1] if outcome else "unclassified"] += 1
    return dict(available=True, codes=dict(codes), outcomes=dict(outcomes))


def evidence_summary(directory, aggregate):
    trials = json.loads((directory / "results.json").read_text())
    manifest = json.loads((directory / "manifest.json").read_text())
    provenance = json.loads((directory / "provenance.json").read_text())
    identity = lambda row: (row["system"], row["task"], row["repetition"])
    expected = {identity(row): row for row in manifest["schedule"]}
    actual = {identity(row): row for row in trials}
    if len(expected) != len(manifest["schedule"]) or len(actual) != len(trials):
        raise ValueError("Duplicate expanded trial identity")
    if expected.keys() != actual.keys():
        raise ValueError("Expanded cohort incomplete or contains unplanned trials")
    if any(
        row["system"] not in LABELS or row["task"] not in WORKLOADS for row in trials
    ):
        raise ValueError("Unknown expanded system or workload")
    for key, row in actual.items():
        if row.get("stop") != "harness_failure" and (
            row.get("model") != MODEL or row.get("seed") != expected[key]["seed"]
        ):
            raise ValueError("Expanded model/seed differs from planned trial")
    # The frozen collector counts desktop tools. Audit raw events for any other
    # attempted tool too: a rejected skill call must not look like a clean run.
    for row in trials:
        path = (
            directory
            / f"{row['repetition']}-{row['task']}-{row['system']}"
            / "events.jsonl"
        )
        extras = {}
        if path.exists():
            for index, line in enumerate(path.read_text().splitlines()):
                event = json.loads(line)
                part = event.get("part", {})
                if event.get("type") == "tool_use" and not part.get(
                    "tool", ""
                ).startswith("desktop_"):
                    extras[part.get("callID", part.get("id", index))] = part
        finished = [
            part
            for part in extras.values()
            if part.get("state", {}).get("status") in ("completed", "error")
        ]
        row["other_tool_attempts"] = len(extras)
        row["model_tool_errors"] = row.get("model_tool_errors", 0) + sum(
            part.get("state", {}).get("status") == "error" for part in finished
        )
        row["finished_model_tool_attempts"] = row.get(
            "finished_model_tool_attempts", 0
        ) + len(finished)
        row["unfinished_model_tool_attempts"] = (
            row.get("unfinished_model_tool_attempts", 0) + len(extras) - len(finished)
        )
    tasks = [task for task in WORKLOADS if any(row["task"] == task for row in trials)]
    report = dict(
        suite="expanded",
        model=MODEL,
        trials=len(trials),
        task_order=tasks,
        systems={},
        manifest=manifest,
        provenance=provenance,
        failures=[],
    )
    resume_path = directory / "resumptions.jsonl"
    report["resumptions"] = (
        [json.loads(line) for line in resume_path.read_text().splitlines() if line]
        if resume_path.exists()
        else []
    )
    report["archived_incomplete_attempts"] = sum(
        len(row.get("archived_incomplete", [])) for row in report["resumptions"]
    )
    report["environment_metadata"] = {
        name: json.loads((directory / name).read_text())
        for name in ("environment.json", "resumption-environment.json")
        if (directory / name).exists()
    }
    for system in LABELS:
        selected = [row for row in trials if row["system"] == system]
        if not selected:
            continue
        native = [row for row in selected if "binary_sha256" in row]
        for field, expected_value in (
            ("binary_sha256", provenance["sources"][system]["binary_sha256"]),
            ("transport", provenance["sources"][system]["transport"]),
            ("build_profile", provenance["sources"][system]["build_profile"]),
        ):
            if any(row[field] != expected_value for row in native):
                raise ValueError(f"Expanded {field} differs from provenance")
        data = extended_aggregate(selected, aggregate)
        data["tasks"] = {
            task: extended_aggregate(
                [row for row in selected if row["task"] == task], aggregate
            )
            for task in tasks
        }
        data["toolkits"] = {
            toolkit: extended_aggregate(
                [row for row in selected if WORKLOADS[row["task"]][0] == toolkit],
                aggregate,
            )
            for toolkit in sorted({WORKLOADS[row["task"]][0] for row in selected})
        }
        data["all_private_runtimes_removed"] = all(
            row.get("private_runtime_removed") for row in selected
        )
        report["systems"][system] = data
        for row in selected:
            if clean(row):
                continue
            tags = []
            if not row.get("success"):
                tags.append("goal not verified")
            if row.get("model_tool_errors"):
                tags.append("tool errors")
            if row.get("other_tool_attempts"):
                tags.append("non-desktop tool attempt")
            if row.get("receiver", {}).get("wrong_submissions"):
                tags.append("wrong submission")
            if row.get("receiver", {}).get("unsafe_action"):
                tags.append("unsafe receiver action")
            if row.get("stop") != "model_finished":
                tags.append(row.get("stop", "unknown stop"))
            if row.get("unfinished_model_tool_attempts"):
                tags.append("unfinished tool")
            report["failures"].append(
                dict(
                    system=system,
                    task=row["task"],
                    seed=row.get("seed", expected[identity(row)]["seed"]),
                    repetition=row["repetition"],
                    goal_verified=bool(row.get("success")),
                    clean=False,
                    tags=tags,
                    tool_errors=row.get("model_tool_errors"),
                    wrong_submissions=row.get("receiver", {}).get("wrong_submissions"),
                    error_classification=error_classification(
                        directory
                        / f"{row['repetition']}-{row['task']}-{system}"
                        / "events.jsonl"
                    ),
                )
            )
    groups = {}
    for row in trials:
        groups.setdefault((row["task"], expected[identity(row)]["seed"]), {})[
            row["system"]
        ] = row
    pairs = [
        (group["updated"], group["upstream"])
        for group in groups.values()
        if set(group) == set(LABELS)
    ]
    for a, b in pairs:
        if a.get("workload") and b.get("workload") and a["workload"] != b["workload"]:
            raise ValueError("Paired workload specifications differ")
    outcomes = Counter(
        "both"
        if a.get("success") and b.get("success")
        else "ours_only"
        if a.get("success")
        else "upstream_only"
        if b.get("success")
        else "neither"
        for a, b in pairs
    )
    matched = [
        (a, b)
        for a, b in pairs
        if a.get("success") and b.get("success") and WORKLOADS[a["task"]][1] != "visual"
    ]
    report["paired"] = dict(
        pairs=len(pairs),
        outcomes=dict(outcomes),
        matched_success_pairs=len(matched),
        matched_tasks=sorted({a["task"] for a, _ in matched}),
        wall=paired_stats(matched, "model_seconds"),
        context=paired_stats(matched, "cumulative_reported_input_tokens"),
        native=paired_stats(matched, "tool_seconds"),
    )
    for system in report["systems"]:
        report["systems"][system]["matched_success"] = extended_aggregate(
            [a if system == "updated" else b for a, b in matched], aggregate
        )
        penalized = [
            min(
                row.get("model_seconds", float("inf")),
                expected[identity(row)]["timeout"],
            )
            if row.get("success")
            else expected[identity(row)]["timeout"]
            for row in trials
            if row["system"] == system
        ]
        report["systems"][system]["capped_completion_seconds"] = stats(penalized)
    return report


def render(report):
    esc = html.escape
    systems = report["systems"]

    def fmt(value, suffix="", precision=1):
        return "—" if value is None else f"{value:,.{precision}f}{suffix}"

    def bars(title, values, suffix="", precision=1, caption=""):
        maximum = max(values.values(), default=0) or 1
        rows = "".join(
            f'<div class="bar-row"><span>{LABELS[key]}</span><div class="track"><i class="{key}" style="width:{100 * value / maximum:.2f}%"></i></div><b>{fmt(value, suffix, precision)}</b></div>'
            for key, value in values.items()
        )
        return f'<section><h2>{esc(title)}</h2><p class="caption">{esc(caption)}</p>{rows}</section>'

    charts = bars(
        "Receiver-verified goals",
        {
            key: 100 * data["successes"] / data["attempts"]
            for key, data in systems.items()
        },
        "%",
        caption="All planned trials; unsupported and failed tasks remain in the denominator.",
    )
    charts += bars(
        "Clean completions",
        {
            key: 100 * data["clean_successes"] / data["attempts"]
            for key, data in systems.items()
        },
        "%",
        caption="Verified goal, no tool errors, wrong submissions, unsafe actions, deadline, or unfinished calls.",
    )
    for title, metric, scale, suffix, caption in (
        (
            "Matched successful semantic tasks · time",
            "model_seconds",
            1,
            "s",
            "Only same-seed pairs where both completed; see coverage below.",
        ),
        (
            "Matched successful semantic tasks · cumulative input",
            "cumulative_reported_input_tokens",
            1000,
            "k",
            "Repeated provider input/cache context, not unique tokens.",
        ),
        (
            "Matched successful semantic tasks · native RPC",
            "tool_seconds",
            1,
            "s",
            "Summed RPC time; not CPU time.",
        ),
    ):
        values = {
            key: data["matched_success"][metric]["mean"] / scale
            for key, data in systems.items()
            if data["matched_success"][metric]["mean"] is not None
        }
        charts += bars(title, values, suffix, caption=caption)
    headers = "".join(f"<th>{LABELS[key]}</th>" for key in systems)
    rows = []
    for task in report["task_order"]:
        cells = []
        for data in systems.values():
            r = data["tasks"][task]
            cells.append(
                f"<td>{r['successes']}/{r['attempts']} <small>clean {r['clean_successes']}</small><br><small>{fmt(r['model_seconds']['median'], 's')} median · {fmt(r['model_seconds']['p90'], 's')} p90<br>{r['model_tool_errors']} errors · {r['wrong_submissions']} wrong</small></td>"
            )
        toolkit, _, name = WORKLOADS[task]
        rows.append(
            f"<tr><td>{esc(name)}<br><small>{esc(toolkit)}</small></td>{''.join(cells)}</tr>"
        )
    timing_rows = []
    for title, field in (
        ("Provider-request union / task", "provider_seconds_union"),
        ("Native RPC union / task", "native_tool_seconds_union"),
        ("Unattributed remainder / task", "unattributed_seconds"),
        ("First reasoning delta / request", "first_reasoning_delta_seconds"),
        ("First output delta / request", "first_output_delta_seconds"),
    ):
        timing_rows.append(
            f"<tr><td>{title}</td>"
            + "".join(
                f"<td>{fmt(data['timing'][field]['mean'], 's', 2)}</td>"
                for data in systems.values()
            )
            + "</tr>"
        )
    error_rows = "".join(
        f"<tr><td>{LABELS[key]}</td><td>{data['model_tool_errors']}/{data['finished_model_tool_attempts']}</td><td>{data['wrong_submissions']}</td><td>{data['unsafe_actions']}</td><td>{data['unfinished_model_tool_attempts']}</td></tr>"
        for key, data in systems.items()
    )
    failure_rows = "".join(
        f"<tr><td>{LABELS[row['system']]} · {esc(row['task'])} · {row['seed']}</td><td>{esc(', '.join(row['tags']))}</td></tr>"
        for row in report["failures"]
    )
    paired = report["paired"]
    captures = " · ".join(
        f"{LABELS[key]}: {data['timing']['complete']}/{data['attempts']}"
        for key, data in systems.items()
    )
    caps = " · ".join(
        f"{LABELS[key]}: {fmt(data['capped_completion_seconds']['mean'], 's')}"
        for key, data in systems.items()
    )
    sources = " · ".join(
        f"{LABELS[key]}: {report['provenance']['sources'][key]['build_profile']}/{report['provenance']['sources'][key]['transport']}"
        for key in systems
    )
    interval = paired["wall"].get("descriptive_stratified_bootstrap_95")
    pair_text = f"{paired['matched_success_pairs']} matched successful semantic pairs; families: {', '.join(paired['matched_tasks'])}."
    if interval:
        pair_text += f" Ours − upstream mean wall difference {fmt(paired['wall']['mean_difference'], 's')}; descriptive within-family bootstrap range {fmt(interval[0], 's')} to {fmt(interval[1], 's')}."
    teardown = all(data["all_private_runtimes_removed"] for data in systems.values())
    interruptions = (
        f" {report.get('archived_incomplete_attempts', 0)} unterminated attempt(s) from driver interruption are archived separately, not merged into the scheduled trial results. Completed failures were not rerun."
        if report.get("archived_incomplete_attempts")
        else ""
    )
    return f"""<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Expanded headless benchmark</title><style>
*{{box-sizing:border-box}}body{{margin:0;font:13px var(--font-sans,system-ui);color:var(--foreground,CanvasText)}}section{{margin-top:22px}}h2{{font-size:14px;margin:0 0 7px}}p{{line-height:1.55;margin:0}}.caption,small,.notes{{font-size:11px;color:var(--muted-foreground,GrayText)}}.intro{{font-size:12px}}.bar-row{{display:grid;grid-template-columns:140px minmax(20px,1fr) 65px;gap:8px;align-items:center;margin:10px 0;font-size:12px}}.bar-row b{{text-align:right;font-family:var(--font-mono,monospace)}}.track{{height:12px;background:color-mix(in srgb,currentColor 8%,transparent)}}.track i{{display:block;height:12px}}.updated{{background:var(--chart-2,teal)}}.upstream{{background:var(--chart-4,orange)}}table{{width:100%;border-collapse:collapse;font-size:12px}}td,th{{padding:9px 4px;border-bottom:1px solid var(--border,GrayText);text-align:right}}td:first-child,th:first-child{{text-align:left;padding-left:0}}th{{font-weight:500;font-size:11px}}small{{line-height:1.6}}.notes p{{margin-top:10px}}
</style></head><body><p class="intro">{report["trials"]} planned trials · {len(report["task_order"])} workload families · {esc(report["model"])}<br>{esc(sources)}</p>
{charts}<section><h2>Workload results · all attempts</h2><table><tr><th>Workload</th>{headers}</tr>{"".join(rows)}</table></section>
<section><h2>Provider versus native time · all attempts</h2><table><tr><th>Measured phase</th>{headers}</tr>{"".join(timing_rows)}</table><p class="caption">Provider-request duration includes remote inference and network. First output may follow hidden reasoning. The remainder is not assumed to be CPU or model time. Complete stream captures: {esc(captures)}.</p></section>
<section><h2>Errors and receiver mistakes</h2><table><tr><th>System</th><th>Tool errors / finished</th><th>Wrong submits</th><th>Unsafe</th><th>Unfinished</th></tr>{error_rows}</table></section>
<section><details><summary>Non-clean trials · {len(report["failures"])} (expand)</summary><table><tr><th>System / workload / seed</th><th>Observed flags</th></tr>{failure_rows or '<tr><td colspan="2">None</td></tr>'}</table></details></section>
<div class="notes"><p>{esc(pair_text)} Conditioning on joint success excludes capability failures; it is not an overall ranking. Paired outcomes: {esc(str(paired["outcomes"]))}.</p>
<p>Capped completion score (failures charged the planned deadline; lower is better): {esc(caps)}. This is a benchmark score, not actual elapsed time. Four variants/family are insufficient for broad platform claims; bootstrap ranges describe this fixed workload mix, not the population of applications.</p>
<p>GTK/Qt tasks use real callback state; KWrite uses exact saved-file contents. Goal success and clean success are separate. Context is reported provider input plus cache reads/writes summed over steps. No failure or host schema rejection is discarded.{esc(interruptions)} All private runtimes removed: {str(teardown).lower()}; no physical desktop input. Raw synthetic evidence, manifest and hashes are retained locally.</p></div></body></html>"""
