# Computer Use MCP

[![CI](https://github.com/mirsella/computer-use-mcp/actions/workflows/publish.yml/badge.svg)](https://github.com/mirsella/computer-use-mcp/actions/workflows/publish.yml)

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

Package 2.x targets OpenCode v2 (2.0.27 or newer). Package 1.x retains the
OpenCode v1 adapter (1.18 or newer, below 2). Codex uses the same Rust MCP
server and manifests in both releases. The native v2 adapter uses
`@opencode/plugin`; local directories also expose a physical `server` entrypoint.

Install `@mirsella/computer-use-mcp` for OpenCode v2, or `@mirsella/computer-use-mcp@opencode1`
for OpenCode v1. Existing Nix-managed and local-directory installations can keep
their configured paths.

## Requirements

- KDE Plasma Wayland with `kwin_wayland` and the KDE desktop portals
- AT-SPI, PipeWire 1.0 or newer, GLib/GIO, D-Bus, and libxkbcommon runtime libraries
- `dbus-daemon`, PipeWire, WirePlumber, `at-spi-bus-launcher`,
  `at-spi2-registryd`, and `xdg-desktop-portal` for the private-session runner
- An XDG RemoteDesktop portal with EIS support for generated input

The npm package includes a Linux x64 executable built on Ubuntu 24.04 and
requires glibc 2.39 or newer. It needs no Rust toolchain, install scripts, or
binary downloads at installation time. Desktop services and shared libraries
must be installed separately. Other architectures can build from source.

Source builds require Rust 1.97 or newer and development files. Ubuntu 24.04
build dependencies:

```sh
sudo apt-get install build-essential clang libclang-dev libdbus-1-dev \
  libglib2.0-dev libpipewire-0.3-dev libspa-0.2-dev \
  libxkbcommon-dev pkg-config
```

The exact service package names vary by distribution. `doctor` reports missing
runtime prerequisites.

## Install

Run the precompiled npm package with Node.js 22 or newer:

```sh
npx -y @mirsella/computer-use-mcp version
npx -y @mirsella/computer-use-mcp mcp
```

Or build and install the binary from the repository:

```sh
cargo install --locked --git https://github.com/mirsella/computer-use-mcp computer-use-mcp
computer-use-mcp version
```

## Codex and other MCP hosts

OpenCode loads the JavaScript `./server` package export (also available as
the root export). Codex reads the plugin and MCP manifests. Both integrations
launch the same packaged Rust executable.

The server runs independently of OpenCode. Any stdio MCP host can launch
`computer-use-mcp mcp --compact-tools` for help/dispatch, or omit
`--compact-tools` for the six direct tools. No host-specific hooks or session
fields are needed.

The [GitHub release](https://github.com/mirsella/computer-use-mcp/releases)
tarball contains the executable, both integrations, and the skill. Extract it
to a permanent directory and register the packaged Codex plugin:

```sh
codex plugin marketplace add /path/to/computer-use-mcp
codex plugin add computer-use@computer-use-mcp
```

Both integrations enable Computer Use by default. To disable it in Codex,
set `enabled = false` in `[plugins."computer-use@computer-use-mcp"]` in
`~/.codex/config.toml`. In OpenCode, set the plugin option `enabled` to `false`
or disable `mcp.servers.computer_use`.

For direct Codex MCP registration, use the installed executable:

```toml
[mcp_servers.computer_use]
command = "/absolute/path/to/computer-use-mcp"
args = ["mcp", "--compact-tools"]
tool_timeout_sec = 150
```

The npm package is now `@mirsella/computer-use-mcp`; it replaces `@mirsella/opencode-computer-use-mcp`.
To use a GitHub release in OpenCode, set the plugin entry to
`"file:///absolute/path/to/unpacked/computer-use-mcp"` instead of the npm name.

## OpenCode setup

Add the package to your OpenCode configuration, using OpenCode 2.0.27 or newer:

```jsonc
{
  "$schema": "https://opencode.ai/v2/config.json",
  "plugins": ["@mirsella/computer-use-mcp"]
}
```

The native v2 plugin registers `mcp.servers.computer_use` with the bundled executable, a
150-second timeout, and the computer-use skill. It supplies the graphical session
variables the server needs for input that the harness lacks, reading them from the systemd
user manager, such as under a service-started `opencode serve`.
It is enabled by default.

When enabled, the model sees `computer_use_help` and
`computer_use_dispatch`. Help lists operation names or returns one
operation's full schema; dispatch executes it through the same validation
and desktop broker. The skill loads on demand.

Settings:

- `enabled` (default `true`): start the server and load the skill. Set it to
  `false` to disable both. An explicit `mcp.servers.computer_use` entry takes precedence.
- `compactTools` (default `true`): keep the two compact tools instead of the
  six direct tools. The plugin switches to direct tools on its own when
  permission or tool rules, including agent-specific rules, name single
  `computer_use_*` tools, so those policies keep working. Set it to `false`
  to always use direct tools.

Quit and restart OpenCode after adding or updating the plugin.

### Direct MCP configuration

The transport is stdio only. Configure the host to execute
`computer-use-mcp mcp`; stdout is reserved for JSON-RPC and diagnostics use
stderr.

For direct MCP registration without the plugin, use the npm command below.
This example asks for review before each tool call:

```jsonc
{
  "$schema": "https://opencode.ai/v2/config.json",
  "mcp": {
    "servers": {
      "computer_use": {
        "type": "local",
        "command": ["pnpm", "dlx", "@mirsella/computer-use-mcp", "mcp"],
        "disabled": false,
        "codemode": false,
        "timeout": { "execution": 150000 }
      }
    }
  },
  "permissions": [{ "action": "computer_use_*", "resource": "*", "effect": "ask" }]
}
```

For a source installation, replace the command with
`["/absolute/path/to/computer-use-mcp", "mcp"]`.
Append `"--compact-tools"` to either command to use help/dispatch in other MCP
clients. In that mode, host permissions apply to the dispatcher as a whole.

Check the connection with:

```sh
opencode mcp list
```

The broker starts workers lazily. Foreground approval is requested on demand;
known local approval failures retry within a 45-second deadline with bounded
1/2/4/8-second backoff. Cancellation, shutdown, and worker termination stop
the request. Approval exhaustion or revocation retires that worker, making its
IDs stale. An explicit discovery or launch creates a replacement without an
MCP restart. Human activity pauses foreground mutations until 60 seconds of
quiet. Use `wait_for` with `condition: {"type":"human_idle"}`, then observe
fresh state before acting; ordinary human activity does not require a restart.

`list_desktop`, `launch_application`, and targetless `window_opened` accept the
optional `desktop` value `foreground` or `background`. Omit it for foreground
when no returned identity routes the call. Returned opaque IDs and cursors
route later calls; callers do not provide a `session_id`.
Choose background only when the user requests it. The foreground-only
`human_idle` wait accepts neither `desktop` nor `target`.

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

## Releases

CI builds and tests the distributable tarball on every push. Run the `release`
workflow from `main` with the full tested source commit: `main` for 2.x, or
`release/opencode-v1` for 1.x. The workflow verifies that commit and downloads
its checksummed CI artifact instead of rebuilding it. Dispatch with
`npm_mode=publish` to publish through npm's OIDC trusted publisher, create the
GitHub release, and update the Codex marketplace branch for `latest`. Use
`npm_mode=dry-run` to verify without writing. The channels are `latest` for 2.x
and `opencode1` for 1.x.
