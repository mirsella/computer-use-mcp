#!/usr/bin/env python3
"""Resume missing scheduled trials without changing the frozen inner harness."""

import argparse
import datetime
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "desktop_bench", ROOT / "scripts/bench-desktops.py"
)
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


def key(row):
    return row["repetition"], row["task"], row["system"]


def save_results(directory, summaries):
    pending = directory / "results.pending.json"
    pending.write_text(json.dumps(summaries, indent=2, ensure_ascii=False) + "\n")
    pending.replace(directory / "results.json")


def assert_stopped(case):
    """Refuse to repeat a partial case while any known actor may still own it."""
    log = (case / "runner.log").read_text() if (case / "runner.log").exists() else ""
    displays = set(re.findall(r"display=(wayland-virtual-[0-9]+)", log))
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            fds = [str((proc / "fd" / str(fd)).readlink()) for fd in (1, 2)]
            if any(value.startswith(str(case) + "/") for value in fds):
                raise RuntimeError(
                    f"Partial trial still owns an active output descriptor: pid {proc.name}"
                )
            env = (proc / "environ").read_bytes().split(b"\0")
            if any(
                f"WAYLAND_DISPLAY={display}".encode() in env for display in displays
            ):
                raise RuntimeError(
                    f"Partial private session still has a process: pid {proc.name}"
                )
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    for marker in Path("/tmp").glob("computer-use-mcp-isolated-*/isolation.ready"):
        try:
            if any(
                f"launcher_pid={display.rsplit('-', 1)[1]}\n" in marker.read_text()
                for display in displays
            ):
                raise RuntimeError(
                    "Partial trial still has an isolation marker; investigate cleanup first"
                )
        except FileNotFoundError:
            continue


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--updated", type=Path, required=True)
    parser.add_argument("--upstream", type=Path, required=True)
    parser.add_argument("--stopped-worker", type=int, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    directory = args.directory.resolve()
    if Path(f"/proc/{args.stopped_worker}").exists():
        raise RuntimeError("Original worker still exists; do not start a second driver")
    manifest = json.loads((directory / "manifest.json").read_text())
    provenance = json.loads((directory / "provenance.json").read_text())
    if manifest["suite"] != "expanded" or manifest["model"] != bench.MODEL:
        raise RuntimeError("Resume supports the exact expanded benchmark only")
    paths = {
        "harness_sha256": ROOT / "scripts/bench-desktops.py",
        "fixture_sha256": ROOT / "scripts/fixtures/deep-desktop-bench.py",
        "workload_sha256": ROOT / "scripts/desktop_bench_workloads.py",
        "timing_sha256": ROOT / "scripts/bench_provider_timing.py",
    }
    if any(bench.digest(path) != manifest[name] for name, path in paths.items()):
        raise RuntimeError(
            "Frozen instrumentation changed; refusing to combine measurements"
        )
    binaries = {name: getattr(args, name).resolve() for name in ("updated", "upstream")}
    if any(
        bench.digest(binary) != provenance["sources"][name]["binary_sha256"]
        for name, binary in binaries.items()
    ):
        raise RuntimeError("Measured native executable changed")
    summaries = json.loads((directory / "results.json").read_text())
    completed = {key(row) for row in summaries}
    if len(completed) != len(summaries):
        raise RuntimeError("Duplicate completed trial")
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    recovery = dict(
        at=stamp,
        source_sha256=bench.digest(Path(__file__)),
        stopped_worker=args.stopped_worker,
        preserved_trials=len(summaries),
        archived_incomplete=[],
        adopted_completed=[],
    )
    for trial in manifest["schedule"]:
        if key(trial) in completed:
            continue
        case = directory / f"{trial['repetition']}-{trial['task']}-{trial['system']}"
        if not case.exists():
            continue
        assert_stopped(case)
        if (case / "summary.json").exists():
            row = json.loads((case / "summary.json").read_text())
            if (
                key(row) != key(trial)
                or row["seed"] != trial["seed"]
                or row["binary_sha256"]
                != provenance["sources"][trial["system"]]["binary_sha256"]
            ):
                raise RuntimeError(
                    "Uncommitted summary does not match scheduled evidence"
                )
            row["private_runtime_removed"] = not Path(
                row["isolation_marker"]
            ).parent.exists()
            if not row["private_runtime_removed"]:
                raise RuntimeError("Completed case still has a private runtime")
            summaries.append(row)
            completed.add(key(row))
            recovery["adopted_completed"].append(case.name)
        else:
            archive = directory / "interrupted-attempts" / stamp / case.name
            archive.parent.mkdir(parents=True, exist_ok=True)
            case.rename(archive)
            recovery["archived_incomplete"].append(
                dict(
                    case=case.name,
                    path=str(archive.relative_to(directory)),
                    reason="driver disappeared during session interruption; no terminal summary; cause not independently established",
                )
            )
    # Adoption can finish the cohort without executing another trial. Commit
    # those terminal summaries before a loop that may have no iterations.
    save_results(directory, summaries)
    with (directory / "resumptions.jsonl").open("a") as log:
        log.write(json.dumps(recovery) + "\n")
    print(json.dumps(recovery), flush=True)
    if len(completed) == len(manifest["schedule"]):
        return
    with bench.benchmark_credentials() as credentials:
        for trial in manifest["schedule"]:
            if key(trial) in completed:
                continue
            system = trial["system"]
            source = provenance["sources"][system]
            case = directory / f"{trial['repetition']}-{trial['task']}-{system}"
            case.mkdir(mode=0o700)
            command = [
                str(ROOT / "scripts/run-isolated-session.sh"),
                "--width",
                "1280",
                "--height",
                "800",
                "--",
                sys.executable,
                str(ROOT / "scripts/bench-desktops.py"),
                "--inside",
                "--output",
                str(case),
                "--credentials",
                str(credentials),
                "--system",
                system,
                "--task",
                trial["task"],
                "--binary",
                str(binaries[system]),
                "--timeout",
                str(trial["timeout"]),
                "--repetition",
                str(trial["repetition"]),
                "--ours-transport",
                provenance["sources"]["updated"]["transport"],
                "--updated-build-profile",
                provenance["sources"]["updated"]["build_profile"],
                "--suite",
                "expanded",
                "--seed",
                str(trial["seed"]),
                *(["--provider-timing"] if manifest["provider_timing"] else []),
            ]
            with (case / "runner.log").open("wb") as log:
                process = subprocess.Popen(command, stdout=log, stderr=log)
                try:
                    process.wait(timeout=trial["timeout"] + 150)
                except subprocess.TimeoutExpired:
                    bench.terminate(process)
                finally:
                    bench.terminate(process)
            if (case / "summary.json").exists():
                row = json.loads((case / "summary.json").read_text())
                row["private_runtime_removed"] = not Path(
                    row["isolation_marker"]
                ).parent.exists()
                if not row["private_runtime_removed"]:
                    raise RuntimeError("Resumed private runtime not removed")
            else:
                row = dict(
                    system=system,
                    task=trial["task"],
                    repetition=trial["repetition"],
                    success=False,
                    stop="harness_failure",
                    exit_code=process.returncode,
                )
            summaries.append(row)
            completed.add(key(trial))
            save_results(directory, summaries)
            print(
                json.dumps(
                    {
                        name: row.get(name)
                        for name in (
                            "system",
                            "task",
                            "repetition",
                            "success",
                            "model_seconds",
                            "stop",
                        )
                    }
                ),
                flush=True,
            )


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    main()
