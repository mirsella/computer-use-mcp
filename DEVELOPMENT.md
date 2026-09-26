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
handoff latching, operation cleanup, and physical-modifier refusal. No unit test
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
```

This invokes the binary directly, without an outer isolated-session wrapper or
injected private environment.

The confirmed run kept the foreground worker count at zero, reused the
persistent background worker, launched on the background route, and passed
PNG capture, pointer click, focused typing, portal Clipboard paste, and fresh
AT-SPI readback. The PNG was 1280x720 and 32,121 bytes; the pointer click
targeted `NewFile`, and both readback strings matched. It exited successfully
with the target gone and no owned descendants. The window-close case was
unavailable because the AT-SPI target had no KDE authority.

The installed Rust 1.97.0 workspace verification passed 346 tests: 312 library,
3 cancellation, 4 contract, 6 isolated, 3 process, 4 readiness, and 14
validation tests, with 1 ignored. Clippy passed. This is evidence for the tested
Linux/KDE environment, not a claim of support for every platform.

## Entry points

### npm packaging and releases

`@mirsella/opencode-computer-use-mcp` ships the Linux x64 GNU executable, an
OpenCode plugin, and a copy of the canonical skill. `npm pack` builds the
release binary and stages those files. Cargo and npm versions must match.
The package has no JavaScript dependencies or install-time scripts.

```sh
npm pack
npm test
```

The publish workflow builds on Ubuntu 24.04, tests the extracted tarball's
plugin and MCP handshake without a desktop, and uploads the tarball as the
`npm-package` artifact. Pushes, pull requests, and manual workflow runs build
and test only. Publishing a GitHub release with a matching `v<version>` tag
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
