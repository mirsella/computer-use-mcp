# Computer Use MCP for Linux Wayland

[![CI](https://github.com/mirsella/computer-use-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mirsella/computer-use-mcp/actions/workflows/ci.yml)

A local [Model Context Protocol](https://modelcontextprotocol.io/) server for
computer use on KDE Plasma Wayland. It is designed for a trusted local MCP
host and user.

> [!WARNING]
> AT-SPI reads and semantic actions can expose private text or cause side
> effects without a portal prompt. Portal approval is session-level, not
> per-call. A screenshot contains the complete selected monitor, including
> unrelated windows and notifications.

See [MCP.md](MCP.md) for setup and the six-tool public contract,
[SECURITY.md](SECURITY.md) for trust boundaries, and
[ARCHITECTURE.md](ARCHITECTURE.md) for implementation authorities.

## Requirements

- KDE Plasma Wayland with `kwin_wayland` and the KDE desktop portals
- Rust 1.97 or newer using the ambient toolchain
- AT-SPI, PipeWire, SPA, GLib/GIO, D-Bus, and libxkbcommon development files
- `dbus-daemon`, PipeWire, WirePlumber, `at-spi-bus-launcher`,
  `at-spi2-registryd`, and `xdg-desktop-portal` for the private-session runner
- An XDG RemoteDesktop portal with EIS support for generated input

Ubuntu 24.04 build dependencies:

```sh
sudo apt-get install build-essential clang libclang-dev libdbus-1-dev \
  libglib2.0-dev libpipewire-0.3-dev libspa-0.2-dev \
  libxkbcommon-dev pkg-config
```

The exact service package names vary by distribution. `doctor` reports missing
runtime prerequisites.

## Install

Build and install the binary from the repository:

```sh
cargo install --locked --git https://github.com/mirsella/computer-use-mcp computer-use-mcp
computer-use-mcp version
```

## OpenCode Setup

The transport is stdio only. Configure the host to execute
`computer-use-mcp mcp`; stdout is reserved for JSON-RPC and diagnostics use
stderr.

Register the binary with OpenCode:

```sh
opencode mcp add computer_use -- "$(command -v computer-use-mcp)" mcp
```

Keep the absolute command path, set the host timeout to `90000`, and require
review for the six tools. For OpenCode, use:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "computer_use": {
      "type": "local",
      "command": ["/absolute/path/to/computer-use-mcp", "mcp"],
      "enabled": true,
      "timeout": 90000
    }
  },
  "permission": { "computer_use_*": "ask" }
}
```

Check the connection with:

```sh
opencode mcp list
```

The broker starts workers lazily. Foreground approval is requested on demand;
known local approval failures retry within a 45-second deadline with bounded
1/2/4/8-second backoff. Cancellation, shutdown, and worker termination stop
the request. Approval exhaustion or revocation retires that worker, making its
IDs stale. An explicit discovery or launch creates a replacement without an
MCP restart. Takeover interruption is the exception: it remains latched and
requires user authorization and an MCP restart.

`list_desktop`, `launch_application`, and targetless `window_opened` accept the
optional `desktop` value `foreground` or `background`. Omit it for foreground
when no returned identity routes the call. Returned opaque IDs and cursors
route later calls; callers do not provide a `session_id`.

The direct normal-MCP smoke passed screenshot, click, focused typing, portal
Clipboard paste, and fresh AT-SPI readback. It kept the foreground worker count
at zero, reused the persistent background worker, and verified clean teardown.
The window-close case was unavailable because the AT-SPI target had no KDE
authority.

## Private Sessions

The supported isolated-session entry point is:

```sh
scripts/run-isolated-session.sh [--width 1920] [--height 1080] [--scale 1] [--] [command...]
```

It starts and verifies private D-Bus, KWin, PipeWire, WirePlumber, AT-SPI, and
portal services. It requires `kwin_wayland`; it does not fall back to cage or
gamescope. The process binds the actual `WAYLAND_DISPLAY`, and the server
rejects a conflicting `COMPUTER_USE_MCP_DISPLAY` diagnostic value. Isolation
separates routing for cooperative same-user processes; it is not a malicious
same-user sandbox.

The default command is `computer-use-mcp mcp`. A diagnostic check that does not
request portal consent is:

```sh
scripts/run-isolated-session.sh -- computer-use-mcp doctor
```

Normal MCP does not require this wrapper for background operation. The broker's
background worker launches the embedded private runner, owns its private bus
and services, and installs private KDE RemoteDesktop authorization without
changing physical-session permissions.

## Direct Commands

```sh
computer-use-mcp doctor
computer-use-mcp init
```

`doctor` checks prerequisites and session binding without requesting consent.
`init` requests a reusable KDE portal grant. The `call FILE` diagnostic batch
interface uses production validation and runtime; see
[MCP.md](MCP.md#direct-commands-and-troubleshooting).

## Current Limitations

- The private-session `doctor` smoke and direct normal-MCP smoke passed. The
  normal smoke verified launch, capture, pointer click, focused typing, portal
  Clipboard paste, fresh AT-SPI readback, and clean teardown. Its window-close
  case was unavailable because the AT-SPI target had no KDE authority.
- The capture path negotiates `MemFd` or `MemPtr`; native GPU DMA-BUF import is
  unavailable and requires an explicit safe design.
- Paste uses the portal Clipboard capability when negotiated, with bounded
  `text/plain` transfer, `Ctrl+V`, completion, and selection clearing. If the
  capability is unavailable, it uses simulated EIS typing. It does not read or
  restore the previous clipboard.
- AT-SPI-only windows do not provide compositor desktop IDs, so exact
  `window_opened` matching cannot use them or fall back to titles.
- `changed_rect` compares consecutive captured frames, not model observations;
  the first baseline is unknown. A complete baseline can still be returned if
  a new stream has not produced a newer frame during startup capture.
- Protected-surface detection remains heuristic.

KDE Plasma Wayland is the maintained target. X11, macOS, Windows, browser
automation, and a standalone desktop UI are not supported.

## References

- [MCP.md](MCP.md): setup, tools, results, and troubleshooting
- [SECURITY.md](SECURITY.md): trust boundary and residual risks
- [ARCHITECTURE.md](ARCHITECTURE.md): implementation and invariants
- [DEVELOPMENT.md](DEVELOPMENT.md): contributor workflow and checks
