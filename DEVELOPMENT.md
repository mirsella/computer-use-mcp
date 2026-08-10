# Development

## Build and checks

The workspace uses stable Rust with MSRV 1.97 and exact dependency versions.
Native packages are listed in [README.md](README.md#requirements). KDE Plasma
Wayland is the maintained target; a headless CI build does not establish support
for another desktop.

Run from the workspace root:

```sh
cargo fmt --all -- --check
bash scripts/check-guidance.sh
cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
cargo test --locked --workspace --all-features
```

If host Cargo configuration injects nightly-only flags, set
`CARGO_HOME=/tmp/opencode/computer-use-mcp-cargo-home`.

## Contributor invariants

- Keep the six ordered tools, closed schemas, descriptions, and typed validation
  aligned. Preserve opaque IDs, bounded results, error outcomes, and the absence
  of help tools, prompts, resources, resource templates, and output schemas.
- Keep [MCP.md](MCP.md) authoritative for public behavior. Architecture and
  security details belong in their dedicated documents.
- Launch only exact installed, case-sensitive `.desktop` IDs. Never add command,
  argument, clipboard, X11, subprocess, `/dev/uinput`, portal `Notify*`, or
  guessed-geometry escape hatches.
- AT-SPI selected text must be range-capped before `GetText`; use bounded
  `CurrentValue` because the Value interface has no ranged text member.
- Keep Wayland inventory on its blocking event thread. The standard
  foreign-toplevel backend is conservative; the opt-in KDE backend requires
  protocol version 17+ and may be unavailable because it is single-client.
  Isolate that failure from AT-SPI and capture. KDE logical geometry is
  diagnostic only.
- Screenshot coordinates cover the complete approved monitor. Never derive them
  from AT-SPI extents. Spatial actions require the exact source frame and live
  mapping; semantic actions may omit `frame_id`.
- Keyboard input requires a visible PNG point and either 1–8 press events or one
  non-empty type event of at most 4,096 scalars. Keep focus click and keys in one
  cleanup-safe EIS transaction, with synchronization between press phases. An
  AT-SPI focused element is not keyboard authority.
- Revalidate frame, stream, portal session, mapping, target identity, and cache
  generation around generated-input awaits. Do not add whole-monitor
  `change_epoch` staleness: animation makes it an invalid authority.
- Keep the workspace and packaged native skill byte-identical; verify with
  `bash scripts/check-guidance.sh`.

## Test policy

Tests must be deterministic and must not require a live desktop. Use adapter,
portal, capture, input, and runtime fakes to cover identity generations,
relocation, bounded traversal/formatting, pagination, frame conversion and
budgets, portal/session lifecycle, EIS routing and cleanup, cancellation, and
shutdown. Preserve coverage for corrupted/wrapped PipeWire buffers, format
changes, restore-token replacement, stream exhaustion, post-frame identity
revalidation, complete input transactions, and post-action observations.

An ignored, non-mutating discovery check is available:

```sh
cargo test -p computer-use-mcp live_discovery_is_non_mutating -- --ignored
```

Manual MCP checks may list and observe a non-sensitive app with explicit portal
consent. Never automate live click, typing, or other generated input.

## Integration

Follow [README.md](README.md#install) for installation and registration. `mcp`
is the only MCP entry point; `doctor` is non-consenting, `init` requests portal
approval, and `call FILE` uses production validation/runtime for static
diagnostics. Static arrays cannot feed returned opaque IDs into later entries.
