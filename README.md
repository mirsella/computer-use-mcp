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

See [MCP.md](MCP.md) for setup and the canonical six-tool contract, including
state, coordinates, results, and recovery rules.

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

## OpenCode setup

The transport is stdio only. Configure the host to execute
`computer-use-mcp mcp`; stdout is reserved for JSON-RPC.

You may request a reusable KDE portal grant before registration:

```sh
computer-use-mcp init
```

This optional step requests a reusable KDE portal grant.

Register the binary with OpenCode:

```sh
opencode mcp add computer_use -- "$(command -v computer-use-mcp)" mcp
```

In the printed config, keep the absolute path, set `timeout` to `90000`, and set
`"computer_use_*": "ask"`. See [MCP configuration](MCP.md#opencode-configuration).

Test the connection:

```sh
opencode mcp list
```

This may open the portal chooser. Restart or re-enable after portal denial,
timeout, revocation, or stream loss.

## Direct commands

Use `computer-use-mcp help` for all commands. Common diagnostics are:

```sh
computer-use-mcp doctor
computer-use-mcp init
```

The diagnostic `call FILE` batch interface is documented in
[MCP.md](MCP.md#direct-commands-and-troubleshooting).

## Security and support

KDE Plasma Wayland is the maintained target. The project does not support X11,
macOS, Windows, browser automation, or a standalone desktop UI.

- [MCP.md](MCP.md): setup, tool contract, results, and troubleshooting
- [SECURITY.md](SECURITY.md): threat model and residual risks
- [ARCHITECTURE.md](ARCHITECTURE.md): implementation and invariants
- [DEVELOPMENT.md](DEVELOPMENT.md): contributor workflow
