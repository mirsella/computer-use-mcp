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
The compact variant fetches each action schema once, then dispatches all calls
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

The installed Rust 1.97.0 workspace verification passed 370 tests: 334 library,
3 cancellation, 5 contract, 6 isolated, 3 process, 4 readiness, and 15
validation tests, with 1 ignored. Clippy passed. This is evidence for the tested
Linux/KDE environment, not a claim of support for every platform.

## Entry points

### npm packaging and releases

`@mirsella/opencode-computer-use-mcp` ships the Linux x64 GNU executable, an
OpenCode plugin, and a copy of the canonical skill. `npm pack` uses
`cargo install` to build the release executable into `vendor/bin` and copies
the canonical skill. Cargo and npm versions must match.
The package has no JavaScript dependencies or install-time scripts.

```sh
npm pack
npm test
```

The single CI/publish workflow runs formatting, Clippy, and Rust tests on
Ubuntu 24.04, then tests the extracted tarball's
plugin permission selection, direct/compact MCP discovery, and dispatch
validation without a desktop, and uploads the tarball as the
`npm-package` artifact. Branch pushes, pull requests, and manual workflow runs
build and test only. Publishing a GitHub release with a matching `v<version>` tag
publishes that tested artifact to npm. Prereleases use the `next` dist-tag;
stable releases use `latest`.

npm trusted publishing authorizes GitHub repository
`mirsella/computer-use-mcp`, workflow `publish.yml`, with direct publishing
allowed and no environment name. The publishing job uses `id-token: write`;
it needs no npm token secret. The initial package is bootstrapped with a
manually authenticated prerelease before registering the trusted publisher.

### Native CLI

`computer-use-mcp mcp` is the only MCP transport. `doctor` reports diagnostics
without portal consent, `init` requests a reusable KDE portal grant, and
`call FILE` runs production validation and runtime against a static batch.
Static batch entries cannot feed opaque IDs returned by earlier entries into
later entries. See [README.md](README.md#install) for installation and host
registration.
