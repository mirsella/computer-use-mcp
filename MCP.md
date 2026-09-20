# MCP Guide

This is the caller-facing guide for `computer-use-mcp mcp`. The server exposes
exactly six tools over stdio: `list_desktop`, `launch_application`,
`activate_window`, `observe`, `act`, and `wait_for`. The initialize response and
`tools/list` entries are the authoritative schemas and descriptions.

See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation boundaries and
[SECURITY.md](SECURITY.md) for the trust boundary.

## Trust and Startup

Run the server only for a trusted local MCP host and user. AT-SPI reads and
semantic actions do not require portal approval. Portal approval grants a
session rather than each individual call. By default, a screenshot contains the complete
selected monitor, including unrelated applications and notifications.

The transport is stdio. The broker starts idle and creates a worker only when a
call needs a desktop:

```text
computer-use-mcp mcp
```

The host owns stdin and stdout. Do not wrap the command with a program that
writes to stdout. JSON-RPC uses stdout and diagnostics use stderr.

Startup requires `XDG_SESSION_TYPE=wayland`, a live Wayland display, and a
present display socket. The server binds the actual `WAYLAND_DISPLAY`.
`COMPUTER_USE_MCP_DISPLAY` is diagnostic validation only; it cannot redirect
client libraries. If it differs from `WAYLAND_DISPLAY`, startup fails.

The foreground worker requests portal approval on demand. Known local approval
failures retry for up to 45 seconds with 1, 2, 4, and 8 second backoff; call
cancellation, broker shutdown, and worker termination stop the attempt. A
background request starts a private worker inside the embedded runner and does
not require wrapping normal MCP in `scripts/run-isolated-session.sh`. Revocation
or exhausted approval retires only that worker. Its IDs become stale, and the
next explicit discovery or launch starts a replacement worker. A successful
`doctor` check establishes startup support, not unattended capture or input
support.

The direct normal-MCP smoke passed screenshot, click, focused typing, portal
Clipboard paste, and fresh AT-SPI readback. It kept the foreground worker count
at zero, reused the persistent background worker, and verified clean exit with
the target gone and no owned descendants. Its window-close case was unavailable
because the AT-SPI target had no KDE authority; this does not change the exact
authority rules described below.

### OpenCode Configuration

```sh
opencode mcp add computer_use -- "$(command -v computer-use-mcp)" mcp
```

Keep the absolute binary path and use a `90000` millisecond host timeout.
Choose permissions for your host; this example asks before each call:

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

OpenCode applies the last matching permission rule. Check registration with
`opencode mcp list`.

### Private Sessions

Run the server in the supported private session when routing separation is
needed:

```sh
scripts/run-isolated-session.sh [--width 1920] [--height 1080] [--scale 1] [--] [command...]
```

The runner starts and verifies private instances of D-Bus, `kwin_wayland`,
PipeWire, WirePlumber, the AT-SPI bus and registry, and the KDE and generic
portals. It requires `kwin_wayland`; there is no cage or gamescope fallback.
It uses private runtime, configuration, data, and state directories and writes
the readiness marker only after endpoint and ownership checks pass. The default
command is `computer-use-mcp mcp`.

This is routing and process separation for cooperative same-user processes, not
a malicious same-user sandbox. A same-user process with host access may still
inspect or interfere with the session. Normal MCP calls that select
`desktop: "background"` start the embedded private worker automatically; no
host wrapper or private-session configuration is required for that route.

Check the session without requesting portal consent:

```sh
scripts/run-isolated-session.sh -- computer-use-mcp doctor
```

Expect a private `wayland-virtual-*` display, a present socket, and a verified
isolated-session verdict.

### Portal Restore Token

`computer-use-mcp init` opens and closes a temporary KDE approval session and
requests a reusable foreground token. The token is stored under
`$XDG_STATE_HOME/computer-use-mcp/portal-restore-token`, or the equivalent
default XDG state directory. The foreground worker claims and removes it before
use and only stores a replacement returned by a successful portal start.

Set `COMPUTER_USE_MCP_PERSIST_PORTAL=0` before startup to skip token loading and
storage for that run. This does not erase an existing token or revoke a portal
grant. `init` always requests persistence.

## Call Rules

All input objects and nested variants are closed. Unknown fields are rejected.
The server does not advertise prompts, resources, resource templates, or an
`outputSchema`.

- Call `list_desktop` first and copy one complete opaque target exactly.
- Never replace an opaque ID with a title, app name, PID, geometry, traversal
  index, or selector.
- `desktop` is accepted only by `list_desktop`, `launch_application`, and a
  targetless `window_opened` wait. Its values are `foreground` and `background`.
  Omit it to use foreground when no returned identity selects a route.
- Returned targets and cursors route later calls automatically. Do not send a
  `session_id`; it is not part of the caller API. Mixed desktop identities are
  rejected.
- Copy `observation_id`, `frame_id`, and `element_id` unchanged. They are bound
  to a process, observation, or source frame.
- Use only capabilities advertised for the exact target and observation.
- Treat `unknown`, `completed`, stale, unavailable, and timeout results as
  evidence to inspect, not as permission to retry blindly.

Semantic operations may omit a frame. Pointer operations and point-focused
keyboard or paste operations require the exact ready PNG frame named by
`act.source_observation.frame_id`. A semantic focus operation uses an
`element_id`, grabs AT-SPI focus, and verifies focus and active-window state
without moving the pointer.

## Tools

### `list_desktop`

```json
{"scope":"windows"}
```

Use `{"scope":"windows","desktop":"background"}` to discover the
private background desktop. `desktop` is optional and defaults to foreground;
the applications scope accepts the same explicit selector.

Results are paged. `limit` defaults to 50 and is bounded at 100. Pass the
returned opaque `next_cursor` unchanged. Do not reuse a cursor after catalog
membership changes.

The windows result includes process-lifetime `app_instance_id` and
`window_instance_id` values, descriptive metadata, backend authority, and
capability states. Standard foreign-toplevel inventory supplies title and app
ID but not PID, geometry, accessibility, or activation. The optional KDE-rich
authority requires `COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT=1` and a Plasma
window-management global version 17 or newer. It can be unavailable or busy
because it is a single-client protocol; that does not weaken other authorities.

The result reports backend status as `supported`, `unsupported`, `unavailable`,
or `busy`. KDE geometry and desktop fields are logical diagnostics. AT-SPI
extents are not input or crop authority.

Use the other scope for installed applications:

```json
{"scope":"applications"}
```

It returns exact case-sensitive GIO desktop IDs. Launch accepts only one of
those IDs.

### `launch_application`

```json
{"desktop_id":"org.kde.kwrite.desktop"}
```

Add `"desktop":"background"` to launch in the lazy private worker. The
default is foreground.

The ID must come from `list_desktop` with `scope: "applications"`. No command,
path, arguments, or fuzzy name is accepted. The result acknowledges a launch
request; it does not prove that a window has mapped. List windows again or use
`wait_for(window_opened)` to obtain an exact target.

### `activate_window`

```json
{
  "target": {
    "app_instance_id":"app-0000000000000001",
    "window_instance_id":"win-0000000000000002"
  }
}
```

The optional `action` is `activate`, `minimize`, `maximize`, `restore`, or
`close`; the default is `activate`. Activation evidence is authority-specific:
AT-SPI can report a fresh active-state observation, and KDE can report a
matching protocol state transition. Neither proves seat focus or application
delivery. `dispatch.synchronized` reports server synchronization only.

KDE activation may switch one verified KWin desktop before dispatch. An
unknown desktop location does not become proof through a guess. A bounded
AT-SPI desktop hunt may try each non-current desktop once and restore the
original desktop when no active state verifies.

Minimize, maximize, restore, and close require the KDE-rich authority. Their
results distinguish request acceptance, protocol send, and connection flush.
Restore may contain two state subrequests. No window action claims application
effect, seat focus, or client delivery.

Protected surfaces are refused before dispatch with
`ProtectedSurfaceRefused`.

### `observe`

```json
{
  "target": {
    "app_instance_id":"app-0000000000000001",
    "window_instance_id":"win-0000000000000002"
  },
  "view":"both",
  "accessibility":{"scope":"interactive"}
}
```

`view` is `screenshot`, `accessibility`, or `both`. Accessibility scopes are
`full`, `visible`, and `interactive`. Accessibility-only observations do not
wait for portal capture.

The optional crop is `monitor` (default) or `target_window`. Target-window crop
uses KDE logical geometry, never AT-SPI extents. It is applied to raw source
pixels before transform, downscale, and PNG encoding. The result retains the
exact mapping back to the approved EIS region. Missing or untrusted geometry
falls back to the full monitor with an explicit reason.

Screenshot coordinates are half-open PNG pixels. A ready result contains the
exact `frame_id` required for spatial input. Accessibility elements expose
opaque `element_id` values. Text and structured output are bounded and report
truncation without cutting serialized JSON at an arbitrary byte.

Ready screenshots may include `changed_rect`, an advisory tile-hash bounding box
against the previous committed captured frame. It is not a model observation
delta. The first captured frame has no baseline. A new stream may still return
that complete baseline if no newer frame arrives during startup capture.

### `act`

Every call names the exact source observation:

```json
{
  "target": {
    "app_instance_id":"app-0000000000000001",
    "window_instance_id":"win-0000000000000002"
  },
  "source_observation": {
    "observation_id":"obs-0000000000000003",
    "frame_id":"frame-0000000000000004"
  },
  "operation":{"type":"semantic","action":{"type":"invoke"}}
}
```

Semantic operations include `invoke`, `focus`, advertised `named` actions, and
`set_value`. Pointer operations use exact source-PNG coordinates. Keyboard
operations are either 1 to 8 press-only events or one non-empty `type` event of
at most 4,096 Unicode scalar values. Point focus, key events, synchronization,
and cleanup stay in one EIS transaction. The click is sent and flushed before
keys are dispatched.

`paste` focus-clicks an exact source-PNG point. The session requests Clipboard
before `RemoteDesktop.Start`; when the portal returns `clipboard_enabled`, it
uses bounded `text/plain` transfer through `SelectionTransfer` and
`SelectionWrite`, sends `Ctrl+V`, waits for completion, and clears the
selection. It does not read or restore the user's previous clipboard. If the
capability is unavailable, it types bounded 4,096-scalar chunks through the
same cleanup-safe EIS transaction. The direct normal-MCP smoke passed the
portal Clipboard path and readback.

A successfully dispatched click may grant one 15-second focus grace for the
same accessibility app and window when the mapped point lies inside current KDE
geometry. It is best effort, consumed once, and invalidated by focus or
geometry changes. Unknown focus state is reported as unavailable or null.
Semantic typing requires a fresh exact focused-element and active-window
verification. A false `Component.GrabFocus` result is accepted only when that
fresh verification succeeds.

Action results separate dispatch, cleanup, and post-visual or
post-accessibility evidence. They do not prove text delivery, application
effect, seat focus, or client delivery.

### `wait_for`

`target` is optional only for window conditions. `condition` and `timeout_ms`
are required; the timeout is bounded at 5000 milliseconds. Frame,
accessibility, and element conditions require and resolve an exact target.

Window open:

```json
{
  "condition":{"type":"window_opened","desktop_id":"org.kde.kwrite"},
  "timeout_ms":3000
}
```

The requested desktop ID may include or omit `.desktop`. The wait polls bounded
catalog refreshes and matches only exact compositor `app_id`, normalizing that
suffix on both values. It never matches a title or AT-SPI application name. The
condition has presence semantics: if a matching entry is present during the
wait, it returns that entry's opaque target. It does not prove that the entry
arrived after the call. An AT-SPI-only entry cannot satisfy this exact identity
condition because AT-SPI does not expose a compositor desktop ID.

Window close:

```json
{
  "condition":{"type":"window_closed","window_instance_id":"win-0000000000000002"},
  "timeout_ms":3000
}
```

The wait first refreshes an authoritative catalog and requires the exact opaque
ID to be present. A supplied `target`, if any, must contain the same window ID.
Targetless waits return the target observed in the catalog. An ID that was not
observed, or a catalog that is unavailable, is not treated as closure.

Window waits do not subscribe to an event channel. They refresh and poll until
the bounded timeout or an explicit catalog error. A timeout is not proof that
nothing changed outside the catalog authority.

## Outcomes and Takeover

Known-tool argument and runtime failures are normal tool results with
`isError: true` and structured outcome fields:

| Outcome | Meaning | Next step |
| --- | --- | --- |
| `not_started` | Dispatch was blocked before the action. | Follow `recovery`; observe before retrying if needed. |
| `unknown` | Dispatch may have started or completed. | Observe current state; do not retry blindly. |
| `completed` | Dispatch completed but later evidence or cleanup is incomplete. | Observe current state before acting again. |

`retryable` is advisory. The recovery text is the next-step authority.
`ProtectedSurfaceRefused` is non-retryable.

Takeover monitoring is armed only during an active mutation or wait. It uses
readable physical `/dev/input/event*` devices, the cooperative
`COMPUTER_USE_MCP_TAKEOVER=1` flag or handoff file, and EIS refusal caused by
physical shortcut modifiers. Agent EIS events do not appear as physical device
events. Monitoring is skipped only for a verified isolated session.

On detection, held input is released and `UserTakeoverInterrupted` is returned.
The latch remains active for the MCP lifetime. Removing the signal does not
resume operation. Resumption requires user authorization, clearing the signal,
restarting MCP, and taking a fresh observation. Detection is best effort when
no input device is readable and no cooperative signal exists.

## Direct Commands and Troubleshooting

```sh
computer-use-mcp doctor
computer-use-mcp init
opencode mcp list
```

`doctor` reports prerequisites, display binding, isolation verdict, watcher
state, and portal readiness without requesting consent. `init` requests portal
approval. `call FILE` runs production validation and runtime against a static
batch; static entries cannot feed returned opaque IDs into later entries.

| Symptom | Recovery |
| --- | --- |
| Target is stale or unavailable | Call `list_desktop` with `scope: "windows"` and copy a fresh target. |
| A worker is retired after capture denial, revocation, or exhausted approval | Approve the intended monitor if requested, then call explicit `list_desktop` or `launch_application` to create a replacement worker. |
| Screenshot is not ready | Read its reason; an accessibility-only observation may still work. |
| Capability is unavailable or busy | Use a capability advertised by the exact target, or wait for the authority to recover. |
| Coordinates are invalid | Use the returned PNG dimensions and half-open bounds from the exact source frame. |
| Outcome is `unknown` or `completed` | Observe current state before deciding whether another action is needed. |
| Takeover interruption | Stop. Obtain user authorization, clear the signal, restart MCP, and observe again. |
