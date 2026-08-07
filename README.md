# Computer Use MCP for Linux Wayland

[![CI](https://github.com/mirsella/computer-use-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mirsella/computer-use-mcp/actions/workflows/ci.yml)

A local [Model Context Protocol](https://modelcontextprotocol.io/) server for
computer use on Linux Wayland. It is built for KDE Plasma and OpenCode.

> [!WARNING]
> Run this server only for a trusted local MCP host and user. Portal approval
> controls capture and generated input, but it does not gate AT-SPI reads or
> semantic actions. The host can receive accessible text and act on controls in
> the logged-in graphical session. A screenshot contains the complete selected
> monitor, including unrelated windows and notifications.

The server uses AT-SPI for semantic inspection and actions, XDG ScreenCast for
one approved monitor, XDG RemoteDesktop with EIS for input, and GIO for app
discovery and launch. It does not use X11, `/dev/uinput`, clipboard injection,
compositor-private APIs, or guessed display geometry.

See [MCP.md](MCP.md) for the six tools and their exact inputs, results, state
lifecycle, coordinate rules, and error outcomes.

## Requirements

- KDE Plasma Wayland with `xdg-desktop-portal-kde`
- Rust 1.97 or newer
- AT-SPI, PipeWire, SPA, GLib/GIO, D-Bus, and libxkbcommon development files
- An XDG RemoteDesktop portal with EIS support

Ubuntu 24.04 build dependencies:

```sh
sudo apt-get install build-essential clang libclang-dev libdbus-1-dev \
  libglib2.0-dev libpipewire-0.3-dev libspa-0.2-dev \
  libxkbcommon-dev pkg-config
```

## Install

Build and install the binary from the repository:

```sh
cargo install --locked --git https://github.com/mirsella/computer-use-mcp computer-use-mcp
computer-use-mcp version
```

The package name is explicit because this repository is a virtual Cargo
workspace.

## OpenCode setup

The transport is stdio only. Configure the MCP host to execute
`computer-use-mcp mcp`; do not run it interactively or add wrappers that write
to stdout. The host owns stdin and stdout. Stdout contains one UTF-8 JSON-RPC
message per line, while diagnostics go to stderr.

You may request a reusable KDE portal grant before registration:

```sh
computer-use-mcp init
```

This step is optional. It closes the temporary session and succeeds only if KDE
returns a reusable restore token.

Register the binary with OpenCode:

```sh
opencode mcp add computer_use -- "$(command -v computer-use-mcp)" mcp
```

Edit the config path printed by that command. Keep the absolute executable
path, set the local MCP `timeout` to `90000`, and set
`"computer_use_*": "ask"`. The complete config and permission caveats are in
[MCP.md](MCP.md#opencode-configuration).

Test the connection:

```sh
opencode mcp list
```

The status check starts enabled servers temporarily and may open the portal
chooser. Normal OpenCode use starts another process. Portal approval happens
before MCP initialization, so the 90-second timeout is required. Restart or
re-enable the MCP after revocation or stream loss; retrying a tool cannot create
a new portal session.

## Direct commands

Use `computer-use-mcp help` for all commands. Common diagnostics are:

```sh
computer-use-mcp doctor
computer-use-mcp init
computer-use-mcp list-apps
computer-use-mcp snapshot APP
```

`doctor`, `list-apps`, and `snapshot` do not open the portal chooser. The
diagnostic `computer-use-mcp call FILE` batch interface is documented in
[MCP.md](MCP.md#direct-call-command); it is not an MCP transport.

## Security and support

KDE Plasma Wayland is the maintained target. The project does not support X11,
macOS, Windows, browser automation, or a standalone desktop UI.

- [MCP.md](MCP.md): setup, tool contract, results, and troubleshooting
- [SECURITY.md](SECURITY.md): threat model and residual risks
- [ARCHITECTURE.md](ARCHITECTURE.md): implementation and invariants
- [DEVELOPMENT.md](DEVELOPMENT.md): contributor workflow
