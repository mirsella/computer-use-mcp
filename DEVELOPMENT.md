# Development

## Toolchain and dependencies

The workspace tracks stable Rust and declares 1.97 as its MSRV. Workspace
dependencies use exact versions. The AT-SPI stack uses
`atspi`, `atspi-proxies`, and zbus with Tokio compatibility; keep features as
small as upstream permits.

Capture uses ashpd for maintained portal proxies, raw zbus response decoding for
ScreenCast v6 metadata, pipewire-rs directly, and image's PNG encoder. Desktop
launch uses GIO's installed application registry. Input uses reis for the EI
protocol, xkbcommon for the compositor-provided keymap, and rustix for
`CLOCK_MONOTONIC`. Do not add portal `Notify*`, GStreamer, external
screenshot tools, direct input devices, or compositor-specific paths.

Native builds need the packages listed in the
[README requirements](README.md#requirements). CI uses Ubuntu 24.04 and the same
package set.

The installed binary still needs the signed-in Wayland session's D-Bus, AT-SPI,
PipeWire and SPA runtime, libxkbcommon, `xdg-desktop-portal`, and an EIS-capable
RemoteDesktop portal backend. KDE Plasma Wayland with
`xdg-desktop-portal-kde` is the current supported target. Do not infer support
for another desktop from a successful headless CI build.

If the host Cargo config injects nightly-only flags, use an isolated Cargo home:

```sh
export CARGO_HOME=/tmp/opencode/computer-use-mcp-cargo-home
```

## Required checks

Run these commands from the workspace root:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
cargo test --locked --workspace --all-features
```

Tests must use `AccessibilityAdapter` fakes for discovery, every app resolution
tier, window choice, traversal order and bounds, formatting, relocation, cache
generations, semantic actions, literal text, post-action snapshots, and failure
paths. Portal lifecycle, raw metadata, token permissions, frame layout, bounded
latest-frame delivery, geometry, mapping generations, and PNG limits also need
deterministic tests. Cover request cancellation and changed handles,
`Session.Closed`, restricted-FD ownership transfer, restore-token replacement,
 post-format buffer/metadata negotiation, stream failure/exhaustion without a new
 portal request, wrapped SPA
chunks, pixel orientation, and post-frame AT-SPI revalidation. Do not make the
normal test suite depend on a live desktop.

Generated-input coverage includes ConnectToEIS lifecycle state, unambiguous
monitor-region selection, private EIS coordinate normalization, key chord parsing,
keymap resolution, pointer move/click and wheel sequences, resume synchronization, and held-state
cleanup. These tests must stay
local and must not contact a desktop portal.
Keep regression coverage for complete-action emulation transactions, fresh
post-snapshot frames, post-keyboard synchronization, and bounded shutdown.

An ignored, non-mutating live discovery test is available:

```sh
cargo test -p computer-use-mcp live_discovery_is_non_mutating -- --ignored
```

You can also run `computer-use-mcp list-apps` and an MCP `observe` call
against a non-sensitive visible app. The portal chooser needs real user consent.
Do not automate live click, typing, or any other generated input.

## OpenCode MCP integration

Follow the Cargo installation and OpenCode registration in
[README.md](README.md#install), and keep the user-facing contract in
[MCP.md](MCP.md) synchronized with schemas and behavior. `init` is optional
preapproval; MCP startup itself restores or requests the full KDE portal grant.
The binary starts MCP only through the explicit `mcp` subcommand; no-argument launch prints CLI help.
`doctor`, `list-apps`, and `snapshot APP` do not start a portal session.
`call FILE` executes one JSON call object or static array in one production
runtime without duplicating MCP validation or action logic. Static arrays cannot
feed an opaque `state_id` returned by one entry into another; use MCP for
observe-then-act testing.
The MCP API has exactly six tools: `list_applications` (`running` or `installed`),
`launch_application`, `observe`, `act_on_element`, `pointer`, and `keyboard`.
Keep their closed schemas and action unions in `contract.rs` aligned with
`validation.rs`. `observe` accepts full, visible, and interactive views plus an
optional semantic query. The runtime keeps a bounded cache containing at most
the latest state for each exact app/window; observing one target must not stale
unrelated targets. Element, pointer, and keyboard calls must reject missing or
replaced IDs, consume the acted-on state, and return replacement observations
after successful UI actions. Any mutation or cleanup must clear screenshot
mappings for every cached target. Preserve structured observation metadata:
element capabilities; screenshot readiness, reason, dimensions, and coordinate
spaces; and structured runtime error `outcome`, `retryable`, and `recovery`.
`launch_application` must remain restricted to an exact full case-sensitive
desktop ID from the installed listing and must
never grow an arbitrary command or argument escape hatch.

## Generated input

Generated input uses `SemanticRuntime::screenshot_mapping` and the existing
RemoteDesktop session's one-shot EIS connection. Screenshot coordinates always
refer to the complete approved monitor. Never derive a global target from AT-SPI
window or element extents. Tests must use fake backends and must not open a
portal, send desktop input, or run ignored live tests. Add no portal `Notify*`,
X11, clipboard, subprocess, or `/dev/uinput` fallback.
Generated keyboard tools with screenshot-point focus must keep the focus click
and key events in one cleanup-safe EIS transaction. Element-focused keyboard
tools must freshly relocate the exact AT-SPI object, call
`Component.GrabFocus`, confirm the element is focused and its window active,
and then send keys without a pointer click. An app string
alone never proves or changes seat focus.
