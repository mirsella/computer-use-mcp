# Development

## Toolchain and checks

The repository uses the ambient Rust toolchain. There is no
`rust-toolchain.toml`; the current minimum supported Rust version is 1.97.
Use an installed Rust 1.97.0 toolchain for reproducible verification while the
host nightly behavior is investigated.
Native dependencies are listed in [README.md](README.md#requirements). KDE
Plasma Wayland is the maintained desktop target.

Run these commands from the repository root:

```sh
cargo +1.97.0 fmt --all -- --check
bash scripts/check-guidance.sh
cargo +1.97.0 clippy --locked --workspace --all-features --all-targets -- -D warnings
cargo +1.97.0 test --locked --workspace --all-features
```

If the host config selects Cranelift or nightly-only flags, override those for
verification without changing the user's Cargo configuration:

```sh
RUSTC_BOOTSTRAP=1 RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= \
CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm CARGO_PROFILE_TEST_CODEGEN_BACKEND=llvm \
cargo +1.97.0 test --locked --workspace --all-features
```

The bootstrap setting permits the host's Cargo codegen-backend configuration;
the workspace itself requires stable Rust. Use the same environment for Clippy.
The ambient Cranelift setup aborts intentional panic tests, so verify them with
LLVM rather than skipping them.

## Local OpenCode testing

Use the checkout's plugin and a checkout-built executable. Development testing
does not require a Cargo or npm installation into PATH.

```sh
RUSTC_BOOTSTRAP=1 RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= \
CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm \
cargo +1.97.0 build --locked -p computer-use-mcp
mkdir -p vendor/bin
ln -sfn ../../target/debug/computer-use-mcp vendor/bin/computer-use-mcp
node --test tests/npm.test.mjs
```

The symlink is a checkout-only build artifact. The Node tests load the local
plugin and check MCP discovery and validation without a desktop.

Rebuilding does not replace an already running MCP process: it can keep the old,
deleted executable inode. Reconnect only the computer-use server in the affected
OpenCode workspace after rebuilding, then verify `/proc/<pid>/exe` refers to the
current artifact. Use `opencode run --standalone` for model smoke tests so a
long-lived shared service cannot supply a stale server. Finish active smoke tests
before rebuilding; replacing their executable invalidates the provenance check.

To use that build in OpenCode, replace the published computer-use plugin entry
with `"file:///absolute/path/to/computer-use-mcp"`. Point at the checkout
directory, whose package exports resolve the plugin. Quit and restart OpenCode
after changing the configuration. The plugin's 150-second request timeout
covers the 120-second maximum `human_idle` wait.

## Invariants

- Keep the six ordered tools, closed schemas, typed validation, opaque IDs,
  bounded output, and outcome semantics aligned. Public behavior belongs in
  [MCP.md](MCP.md), not in duplicated instructions.
- Keep broker routing and worker lifecycle explicit: foreground and background
  workers are lazy and independently scheduled, returned IDs and cursors route
  later calls, and retired-worker IDs stay stale until a new explicit discovery
  or launch creates a replacement. Do not add a caller `session_id`.
- Launch only an exact installed, case-sensitive `.desktop` ID. Do not add
  command, argument, X11, direct-device, subprocess, or guessed-geometry escape
  paths. Keep clipboard operations on the negotiated portal path or the bounded
  simulated EIS fallback.
- Keep AT-SPI text reads bounded. Use `CurrentValue` for value metadata and
  require the interfaces needed for verified replacement. Replacement values
  must not enter logs.
- Keep the Wayland catalog on its blocking I/O threads. Treat standard and KDE
  authorities separately, preserve explicit unavailable and busy states, and
  never join records by title.
- Background workers must use the embedded private runner and require private
  KDE PermissionStore authorization on their own bus/data. This must not change
  physical-session permissions. Foreground approval retries only within the
  bounded local deadline and stops on cancellation or shutdown.
- Spatial actions require the exact ready source frame and live mapping.
  Semantic actions may omit a frame. Do not turn AT-SPI extents into PNG
  coordinates or add whole-frame change-epoch staleness.
- Preserve cleanup around every generated-input await. Revalidate target,
  source mapping, stream, portal session, format, route, device, and cache
  generations at the existing barriers.
- Keep the workspace skill and packaged skill byte-identical; verify with
  `bash scripts/check-guidance.sh`.

## Tests

Unit and integration tests must be deterministic and must not require a live
desktop. Use the existing adapter, portal, capture, input, session, and runtime
fakes for identity generations, bounded traversal, frame conversion, portal
lifecycle, EIS routing, cancellation, cleanup, and shutdown. Preserve coverage
for wrapped and corrupt PipeWire buffers, format changes, restore-token
replacement, stream exhaustion, exact source-frame revalidation, portal
Clipboard transfer and cleanup, simulated typing fallback, complete input
transactions, window identity, and post-action evidence.

Session and takeover tests use injected environment, filesystem, device, and
counter fakes. They must verify actual display resolution, isolation checks,
busy/idle transitions, handoff clearance, operation cleanup, and physical-modifier refusal. No unit test
starts a compositor. The runner is checked with `bash -n` and fail-closed
readiness assertions.

Useful focused commands are:

```sh
cargo +1.97.0 test -p computer-use-mcp --lib window_
cargo +1.97.0 test -p computer-use-mcp --test validation
cargo +1.97.0 test -p computer-use-mcp --test contract
cargo +1.97.0 test -p computer-use-mcp live_discovery_is_non_mutating -- --ignored
```

The ignored discovery test is non-mutating. Run live input checks in the owned
private session with a disposable application. Physical-desktop interaction
requires the user's explicit request. Normal tests never generate desktop input.

The private-session startup smoke check does not request portal consent:

```sh
scripts/run-isolated-session.sh -- computer-use-mcp doctor
```

It should report a private `wayland-virtual-*` display, a present socket, and a
verified isolated-session verdict. The live doctor smoke has passed, which
establishes startup support. Run the direct normal-MCP smoke with:

```sh
python3 scripts/isolated-mcp-smoke.py --normal-mcp "$PWD/target/debug/computer-use-mcp"
python3 scripts/isolated-mcp-smoke.py --compact-mcp "$PWD/target/debug/computer-use-mcp"
```

This invokes the binary directly, without an outer isolated-session wrapper or
injected private environment.
The compact variant fetches each action reference once, then dispatches all calls
through the compact transport.

For optional model-driven smoke tests, use the checkout build described above:

```sh
python3 -B scripts/opencode-model-smoke.py --model openai/gpt-6.1-sol --variant medium
python3 -B scripts/opencode-model-smoke.py --model openai/gpt-6.1-sol --variant medium --task takeover
```

The model runner requires OpenCode and Bun, uses the existing provider
configuration and authentication, and incurs model usage. `--provider-config`
selects a different JSON/JSONC provider
configuration. Provider settings come from that file, since `debug config`
redacts executable headers and credentials. It loads only the local plugin in a temporary config,
removes physical display/bus access, and asks the model to enter a unique line
in an unsaved background editor. It requires fresh accessibility readback and a
PNG, checks every discovery/launch route, watches for foreground workers, and
checks owned processes and the private runtime disappear after exit. Temporary
config, transcripts, and screenshots are deleted. The JSON report includes
task-wide token counters, repeated calls, tool errors, and wait results.
Ordinary CI runs only its deterministic evidence-checker tests.

Before deleting private state, both model tasks capture the broker history in
`target/model-smoke/<task>-<run-id>/computer-use-mcp/history/calls.jsonl`.
They require one start/terminal pair per public call, correct operation/status
and desktop routing, and no worker duplicates, dummy text, or screenshot data.
The retained metadata-only snapshot supports the normal `history` command by
setting `XDG_STATE_HOME` to the reported `history_state_dir`. The smoke checks
`--last`, `--since`, `--call-id`, and `--errors` against that snapshot. Its report
includes error counts, failed cleanup, unmatched starts, and the five longest
calls. Transcripts and authentication remain disposable.

The takeover task uses the foreground route inside an owned private runner. It
asserts a cooperative handoff, clears it only after an actual `HumanInputBusy`
mutation refusal, and requires a real one-minute `human_idle` wait before resuming.
Complete JSONL events are read incrementally; model prose, observation text, and
partial records cannot clear the handoff signal.
It checks fresh observation before input, exact readback and PNG verification,
and that the same worker remains alive. Physical monitoring is deliberately
unavailable in verified isolation, so the wait must report that fact rather
than claim hardware idle. Both tasks remove their temporary state and owned
processes. The provider JSONC integration test requires Bun; the evidence
checker tests run without OpenCode or Bun.

The checkout's `gpt-6.1-sol` medium history verification passed both tasks.
The editor run took 99.2 seconds and logged 14 public calls in 28 records with
no errors. The takeover run took 163.0 seconds and logged 18 public calls in
36 records. It waited 60.0 seconds, resumed in the same worker, and its only
error was the expected `HumanInputBusy` refusal with `not_started` dispatch and
completed cleanup. Both runs verified exact accessibility readback and PNGs,
with no duplicate records, unmatched starts, content leakage, or failed cleanup.
All owned processes and private runtimes were removed. These tests exercise
cooperative takeover inside isolation, not physical evdev monitoring.

The longest non-wait call was a 4.826-second combined screenshot/accessibility
observation whose text used 15,999 bytes of the output budget. The retained
history identifies it for a later observation-latency investigation; timing
alone does not distinguish capture cost from accessibility traversal.

A private KWrite probe confirmed that `GrabFocus` can return false while the
exact editor remains focused, and that direct text replacement succeeds. With
fresh focus verification handling that refusal, another `gpt-6.1-sol` medium
editor run passed in 84.7 seconds and 16 tool calls with no tool errors. Exact
accessibility readback, a 48,163-byte PNG, and complete private teardown were
verified.

The confirmed run kept the foreground worker count at zero, reused the
persistent background worker, launched on the background route, and passed
PNG capture, pointer click, focused typing, portal Clipboard paste, and fresh
AT-SPI readback. The PNG was 1280x720 and 32,121 bytes; the pointer click
targeted `NewFile`, and both readback strings matched. It exited successfully
with the target gone and no owned descendants. The window-close case was
unavailable because the AT-SPI target had no KDE authority.

The 2026-11-11 Rust 1.97.0 workspace verification passed 432 tests: 385 library,
3 cancellation, 8 contract, 11 isolated, 3 process, 4 readiness, and 18
validation tests, with 1 ignored. Clippy and 28 package tests passed. The reviewed
13 internal-client and 14 normal Python accounting tests remain valid; the
private GTK receiver regression covers five real callback scenarios. These checks and the private
Wayland live regressions cover the tested Linux/KDE environment, not every app
or platform. Model error-rate and timing claims require the separate recorded
benchmark trials below.

## Entry points

### Matched model benchmarks

For the larger seeded comparison, use `--suite expanded --repetitions 4
--seed-base 101 --provider-timing --systems updated upstream`. The twenty-two
families cover GTK3, GTK4, a Qt widget form, a real KWrite saved document,
asynchronous validation, hidden notebook pages, cancellation, filtering,
modal transitions, and screenshot-only click/type and drag. Ten harder
GTK3 families add a menu bar with a submenu that opens a dialog, a collapsed
tree with a same-named decoy row, a second non-modal editor window, a
right-click context menu on a long list with a destructive sibling item, a
slider with a spin button, a long checklist with pre-checked decoys, an
editable table cell, a multi-page wizard with gated navigation, two
same-titled windows, and a form revealed only by a keyboard shortcut.
This creates 176 trials, each in a fresh private Wayland session. Pixel-only
trials have a 90-second cap; other expanded workloads cap at 180 seconds.
Adding a family must not change the seeded parameters of existing ones;
`scripts/test_bench_desktops.py` pins one frozen value.

The provider timing proxy forwards request and response bodies unchanged;
it logs only timestamps, byte counts, SSE event counts, and numeric usage.
Its private model-level endpoint override is necessary because a catalog
model endpoint can override a provider-level endpoint. Account credentials,
the requested model, and the max reasoning variant remain unchanged.
First streamed reasoning and first output delta are separate measurements,
not interchangeable definitions of time to first token. The unassigned
remainder includes startup, local work, and any uninstrumented services;
it is not presented as CPU time. A per-trial capture-completeness flag
checks observed streams against completed model steps.

Expanded reports require the complete recorded schedule. They retain
goal failures and distinguish clean completion from a recovered goal:
clean means no tool errors, wrong submissions, unsafe receiver action,
deadline, or unfinished call. Cost comparisons use only same-seed semantic
pairs where both succeeded, and list the resulting coverage explicitly.
All-attempt medians/p90s, failures, provider/native timing, and a separate
deadline-penalized completion score remain visible. Descriptive paired
bootstrap ranges describe this fixed workload mix, not all applications.

The deterministic large-collection regression also runs the internal stdio client
against a real GTK app, including targeted search, replacement IDs, and an
independently checked button callback:

```sh
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/observation-smoke.mjs
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/observation-smoke.mjs hard
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/expanded-observation-smoke.mjs qt_invoice
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/expanded-observation-smoke.mjs gtk4_form
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/expanded-observation-smoke.mjs settings
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/expanded-observation-smoke.mjs unicode-preflight
scripts/run-isolated-session.sh --width 1280 --height 800 -- node scripts/expanded-observation-smoke.mjs kwrite
```

The expanded toolkit checks use `target/release/computer-use-mcp`. They verify
native Qt top-level-container discovery and exact modal replacements, GTK4
replacement without redundant unsupported focus, real tab navigation, and
Unicode key-event delivery through negotiated EIS Text. The Qt and
settings checks also run the opt-in stateful workflow helper; native-MCP model
comparison trials do not use that helper. The removed adapter's frozen cohort
remains historical evidence only.

`scripts/bench-desktops.py` runs receiver-verified synthetic tasks against an
unchanged checkout, the current executable, and upstream open-computer-use.
Each trial owns a fresh private KWin Wayland session. Both native MCP servers
use their normal foreground route inside that session; no trial uses the
physical desktop. This compares the backends in a common environment, not
their ability to provision that environment. Use the normal-MCP smoke above
to test our broker-managed background desktop separately.

The pinned model is `opencode-go/deepseek-v4.1-flash#max`. The runner exports
existing Go and active Console credentials into a private temporary directory,
imports them into a disposable OpenCode database, and removes them afterward.
It does not change account credentials or global configuration. Raw model
events contain synthetic task data and are retained in the private output
directory, never alongside exported credentials.

```sh
python3 -B scripts/bench-desktops.py \
  --systems baseline updated upstream --tasks easy medium hard visual \
  --repetitions 2 --timeout 180 --output target/desktop-bench-example
```

The default is the eager direct-tool surface used by the initial comparison.
Pass `--ours-transport compact` to measure our plugin's default help/dispatch
surface, including its inline common-operation templates. This applies to our
selected checkouts; upstream remains on its native direct surface. Keep the
transport explicit when comparing runs: reducing catalog overhead is an
interface improvement, not a backend CPU-speed claim.

Each proxy writes a separate `.usage.json` sidecar with child-process
user/system CPU seconds and maximum RSS. These process-tree measurements exclude
KWin and the fixture. RPC elapsed time remains a different metric.

The final frozen 144-trial follow-up used four variants of each expanded family.
Our Rust server verified 44/48 goals with 35 clean runs and 19/427 model-tool errors;
Open Computer Use verified 22/48 with 17 clean runs and 103/683 errors, including
host rejections. The separate adapter verified 44/48 with 34 clean runs and
17/403 errors. Its cumulative input context was 23.7% higher than our compact
interface. That experimental adapter is not part of the package; its frozen
source and results remain in the historical benchmark evidence.
Older frozen cohorts predate the receiver guards for unsaved Personal-tab edits
and inventory-row selection. Their zero recorded unsafe-action counts do not
establish compliance with those constraints. Current collection also records
nonzero model-process exits explicitly and excludes them from clean runs.
All 144 provider captures were complete and private runtimes were removed.
KWrite's final files were correct in all native runs, but each had a wrong
intermediate save and three agents hit the deadline. All Unicode canvas cases
failed. These results do not establish a general before/after speed improvement.

Set `COMPUTER_USE_MCP_PROFILE_EIS_CAPABILITIES=1` only for capability diagnostics.
It logs fixed keyboard/Text interface booleans, not input or server strings.
The backend now binds EIS Text when advertised. It uses Text keysym events only
for typed characters absent from the physical keymap, with exact same-seat,
keyboard and Text-device generation checks. Shortcuts remain physical-only.
A socket-pair protocol test verifies actual keysym press/release messages and
rejects consumed Text-device generations without emitting more input. Four
private canvas probes (seeds 101–104) verified exact Unicode via the receiver's
key events, with no wrong submission. KWin Text UTF-8 uses the input method;
the backend never substitutes that for keyboard events. Do not rewrite a
received keymap or substitute clipboard insertion for the canvas requirement.

After removing the experimental adapter, two new frozen Rust-only expanded
cohorts each reached 48/48 receiver goals. The final cohort had 38 clean runs,
20 errors in 454 finished model-tool attempts, six wrong intermediate
submissions, and three deadlines. All four canvas and all four GTK4-form cases
were clean. Fourteen errors came from KWrite, including blocking modal actions
and malformed/stale agent requests. Cumulative reported input was 4,879,068
tokens, not lower than the earlier 4,298,229-token Rust cohort. Do not claim
zero errors or a general context improvement. All 96 new timing captures and
private-runtime teardown checks completed. Frozen historical adapter evidence
remains separate and is not shipped or runnable through the current harness.

A fresh 2026-11-11 before/after pair used the reviewed receivers and unchanged
model, harness, workloads, seeds, schedule and time caps. Both builds are ours;
Open Computer Use was not rerun. Baseline: 46/48 goals, 38 clean runs, 10 errors
in 408 finished tool attempts and six wrong submissions. Changed build: 48/48
goals, 35 clean runs, 12 errors in 416 attempts and zero wrong submissions.
Blocking modal timeouts disappeared, but cumulative input increased from
3,094,697 to 3,284,431 and model wall time did not improve. All 96 provider
captures and private teardown checks completed. Do not describe this as an
overall token, speed or error-rate win. No transient read-only failures occurred
in the baseline, so no retry mechanism was added.

The KWrite regression now opens Configure with one click identified from the
actual returned PNG, then changes the saving option before the first file save.
It requires Tesseract for test-only OCR; production has no OCR dependency.
All four seeds saved exact contents with zero wrong submissions and no blocking
semantic invoke. Initialize visual input before opening the menu, because
portal setup can dismiss it. Discover a newly published native modal read-only
if it is absent from immediate post-click evidence; never repeat the click.

For production-cost measurements, build with `cargo +1.97.0 build --locked
--release -p computer-use-mcp` (using the host codegen settings above), then pass
`--updated target/release/computer-use-mcp --updated-build-profile release`.
The flag records the declared profile; it does not build or optimize the binary.
The default path/profile is debug. Reports show the profile and reject mixed
profiles within one system, as they do mixed transports or executable hashes.

See the script's binary-path options for alternate checkouts. Baseline builds
must have their own Cargo target directories. Keep every measured executable
unchanged throughout its trials. The upstream Linux executable is built with
Go from `apps/OpenComputerUseLinux`; the runner needs Python GTK 3, OpenCode,
Bun, and the same private-session dependencies as our normal smoke.

Tasks cover exact text replacement, a footer after 2,000 rows, a multi-step
Unicode form and confirmation dialog, and screenshot-grounded pointer plus
generated keyboard input. Fixture callbacks write independent success state.
The model's final prose is not an acceptance criterion. System order rotates
between tasks and repetitions. A small synthetic pilot is not a statistical
ranking or a platform-wide reliability claim.

Each summary records tool calls/errors, cumulative MCP RPC time, UTF-8 tool
text, decoded image and wire bytes, catalog bytes, model steps, provider token
counts, wall time, native binary hashes, and runtime-directory teardown.
Finished model-side attempt/error counts also include host schema rejections
that never reached native MCP. In-flight model calls and native help calls are
reported separately; repeated state updates do not double-count an attempt.
`peak_reported_input_context_tokens` is the maximum provider-reported step
input plus cache read/write tokens; the cumulative field sums those values.
These are not independent local tokenization or image-context measurements.
MCP time includes response transport and is not backend CPU time. Setup and
provider errors must be reported separately from receiver/backend failures.

After the run, `python3 -B scripts/bench-report.py target/desktop-bench-pilot`
writes `report.json` and a self-contained `report.html` chart. It separates
semantic-task costs from pixel-only capability failures and rejects mixed
binary snapshots. Check accounting with `python3 -B scripts/test_bench_desktops.py`.

To compare a complete pre/post expanded experiment, use:

```sh
python3 -B scripts/bench_expanded_compare.py target/expanded-before target/expanded-after --output target/expanded-comparison
```

This requires identical schedules, workload/fixture/instrumentation hashes and
the same reference executable. It retains all goal/clean/error outcomes and
shows temporal before/after costs only with explicit joint-success coverage.
The four-way cost view uses identical seed/task pairs successful in both
implementations in both phases. Provider conditions can still change between
phases; neither this conditioning nor bootstrap ranges make timing causal.

### npm packaging and releases

`@mirsella/computer-use-mcp` ships the Linux x64 GNU executable, an
OpenCode adapter, Codex plugin, and a copy of the canonical skill. `npm pack` uses
`cargo install` to build the release executable into `vendor/bin` and copies
the canonical skill. Cargo and npm versions must match.
The package has no JavaScript dependencies or install-time scripts.

```sh
npm pack
npm test
```

CI runs formatting, Clippy, and Rust tests on Ubuntu 24.04, then tests the
extracted package's OpenCode permissions, Codex registration, direct/compact
MCP discovery, and dispatch validation without a desktop. It uploads the
tested tarball and `SHA256SUMS` as the `npm-package` artifact.

Push the versioned source commit to `main` for 2.x, or to
`release/opencode-v1` for 1.x, and wait for CI. Dispatch `release.yml` from
`main` with the full source `commit` SHA. It verifies the pinned workflow and
source branch, then reuses the checksummed `npm-package` artifact.

Configure npm's trusted publisher as this repository's `release.yml` without
an environment. Dispatch with `npm_mode=publish` to publish through OIDC,
create the GitHub release, and update the Codex marketplace branch for `latest`.
Use `npm_mode=dry-run` to verify without writing. Versions 1.x use `opencode1`;
2.x use `latest`. GitHub releases attach the same tested tarball and
`SHA256SUMS`. Keep npm, Cargo and Codex versions synchronized.

### Native CLI

`computer-use-mcp mcp` is the only MCP transport. `doctor` reports diagnostics
without portal consent, `init` requests a reusable KDE portal grant, and
`call FILE` runs production validation and runtime against a static batch.
Static batch entries cannot feed opaque IDs returned by earlier entries into
later entries. See [README.md](README.md#install) for installation and host
registration.

The model smoke runner's `--task capabilities` is exploratory: its automatic
acceptance verifies editor readback and a PNG only. Its JSON marks the task
incomplete, lists capabilities requiring manual verification, and preserves the
model's assessment. A successful model run is not a full capability pass.
