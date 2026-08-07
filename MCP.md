# MCP guide

This document is the user-facing contract for the `computer-use-mcp mcp`
server. See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation details and
[SECURITY.md](SECURITY.md) for the threat model.

## Trust boundary

Run this server only for a trusted local MCP host and user.

Portal approval controls monitor capture and generated pointer or keyboard
input. It does not control AT-SPI access. A process in the same graphical
session can read text exposed through accessibility APIs and invoke semantic
actions or launch installed apps without a portal prompt. A screenshot contains
the complete selected monitor, including unrelated windows, notifications, and
desktop content.

Use an MCP host that lets the user inspect and deny sensitive calls. Review the
tool, target, and arguments before observations, launches, semantic actions,
pointer input, and keyboard input. Portal approval is a session-level grant, not
approval for each later tool call.

## Transport and startup

The MCP transport is stdio only. Configure the host to execute:

```text
computer-use-mcp mcp
```

The host owns the child process's stdin and stdout. Do not run `mcp`
interactively or wrap it with a command that writes to stdout. Stdout carries
newline-delimited UTF-8 JSON-RPC messages, one message per line with no embedded
newlines. Startup messages and diagnostics go to stderr.

Before starting the MCP protocol, the process asks KDE to restore or approve
one monitor plus pointer and keyboard access. Tools are not available until the
portal session and PipeWire capture are ready. If the chooser is denied or
times out, startup fails. If the grant is revoked or the stream dies later,
screenshots and generated input become unavailable. AT-SPI reads and semantic
actions are not revoked. Restart the MCP process to restore portal-backed
functionality; retrying a tool does not open another chooser.

### OpenCode configuration

```sh
opencode mcp add computer_use -- "$(command -v computer-use-mcp)" mcp
```

Before testing the connection, edit the configuration file path printed by
`opencode mcp add`. Set a 90-second timeout and require approval for every tool
from this server:

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
  "permission": {
    "computer_use_*": "ask"
  }
}
```

Use the path printed by `command -v computer-use-mcp`. OpenCode's default MCP
timeout is shorter than the server's 60-second portal approval deadline. The
longer timeout also covers initial tool discovery and normal requests that wait
for a fresh screenshot.

Then test the connection and inspect status:

```sh
opencode mcp list
```

This command starts enabled servers for the status check and can open the portal
flow. The later OpenCode session starts its own server process. With the
`computer_use` key, OpenCode exposes tools with the `computer_use_` prefix.

The `ask` rule opens an OpenCode permission prompt unless that tool was already
approved with `Always` during the session. The prompt does not guarantee that
the target and all arguments are visible. Inspect the pending tool call details
and choose `Once` when per-call review is required. Reject a call if its target
or arguments cannot be verified.

OpenCode applies the last matching permission rule. If the existing config has
a broader wildcard rule, place `computer_use_*` after it.

## Portal restore token

`computer-use-mcp init` opens a temporary portal session and closes it after
approval. It succeeds only if KDE returns a reusable restore token. KDE may
still show the chooser after revocation, expiration, or a display change.

The private token file is
`$XDG_STATE_HOME/computer-use-mcp/portal-restore-token`, or
`~/.local/state/computer-use-mcp/portal-restore-token` when `XDG_STATE_HOME` is
unset. The process claims and removes the file before reuse, then saves the
replacement token returned by a successful portal start.

Set `COMPUTER_USE_MCP_PERSIST_PORTAL=0` before MCP startup to avoid reading or
storing a token for that run. This setting does not delete an existing file or
revoke a portal-side grant. `computer-use-mcp init` deliberately forces
persistence and does not honor this opt-out.

## Request rules

The server has six tools. Every input schema is a closed object, including each
nested action and focus variant. Unknown fields are rejected.

- An `action` is always an object with a required `type` field, never a string.
- A `state_id` is opaque. Pass it back exactly as returned; do not construct or
  edit it.
- An `element_id` may be a JSON integer or a decimal string. It is scoped to the
  observation that returned it. Results always encode it as a string.
- App and action names are case-sensitive where the schema or tool description
  says they are. For `named`, pass the exact `named_actions[].name`; do not append
  its description or copy combined display text.

The examples below show tool arguments, not full JSON-RPC request envelopes.

## Recommended workflow

1. Call `list_applications` with `running` to find an accessible target, or with
   `installed` to find an exact desktop ID.
2. Call `observe` for the intended app or window.
3. Check the returned element capabilities and `screenshot.ready` before
   choosing an action route.
4. Pass the returned `state_id` and `element_id` or PNG coordinates unchanged.
5. Continue with the replacement observation returned by a successful action.
6. If an error reports `unknown` or `completed`, observe the current UI before
   deciding whether another action is needed.

## Tool inputs

### Discovery and launch

| Tool | Arguments |
| --- | --- |
| `list_applications` | `{"scope":"running"}` or `{"scope":"installed"}` |
| `launch_application` | `{"desktop_id":"org.kde.kwrite.desktop"}` |

The running result contains app names, PIDs, accessible identities, and windows
with titles, identities, and states. The installed result contains `desktop_id`,
display `name`, and `shown`. Desktop IDs are exact and case-sensitive. Launch
accepts no command, path, or arguments, clears the observation cache, and returns
`status: "requested"`, the desktop ID, and name.

### `observe`

Observe by PID, full case-insensitive app name or window title, or a unique
case-insensitive substring. A PID is also a JSON string, and the `PID=<digits>`
token emitted by `list_applications` is accepted unchanged. Only `target` is
required.

```json
{
  "target":"KWrite",
  "view":"interactive",
  "query":"save",
  "text_limit":500,
  "max_tree_nodes":1200,
  "max_tree_depth":64
}
```

| Field | Default | Allowed value |
| --- | --- | --- |
| `view` | `full` | `full`, `visible`, or `interactive` |
| `query` | none | Nonblank string, at most 1000 characters |
| `text_limit` | `500` | Integer from 0 through 100000, or `"max"` |
| `max_tree_nodes` | `1200` | Integer from 1 through 5000 |
| `max_tree_depth` | `64` | Integer from 1 through 128 |

`text_limit` applies per element. `"max"` selects the server cap of 100000
characters per element. Node or depth limits can produce an incomplete tree;
check the text result for a limit warning before concluding that an element is
absent.

`visible` prunes hidden document subtrees such as background browser tabs.
`interactive` further restricts the returned elements to supported actions,
set-value capability, and known interactive roles or states.

`query` is a case-insensitive semantic filter over role, name, value, text,
selected text, states, action names, and action descriptions. It does not search
pixels, change the retained snapshot, renumber element IDs, crop the screenshot,
or redact monitor content.

Capture failure leaves the accessibility observation available with
`screenshot.ready: false` and a reason.

### `act_on_element`

Use only a capability advertised for that element in the same observation. The
common fields are `state_id`, `element_id`, and one of these action objects:

| Operation | `action` |
| --- | --- |
| Primary action | `{"type":"invoke"}` |
| AT-SPI focus | `{"type":"focus"}` |
| Advertised named action | `{"type":"named","name":"menu"}` |
| Editable text or numeric value | `{"type":"set_value","value":"42"}` |

Semantic element actions can work when `screenshot.ready` is `false`. They
still require the retained state and a fresh identity match.

### `pointer`

Use the returned PNG's `screenshot_png_pixels` space and the common `state_id`.

| Operation | `action` |
| --- | --- |
| Move | `{"type":"move","x":320,"y":180}` |
| Click | `{"type":"click","x":320,"y":180,"button":"left","count":1}` |
| Drag | `{"type":"drag","from_x":320,"from_y":180,"to_x":640,"to_y":360}` |
| Scroll | `{"type":"scroll","x":320,"y":180,"direction":"down","steps":2}` |

Buttons are `left`, `right`, or `middle`; the default is `left`. Click `count`
defaults to 1 and ranges from 1 through 3. Scroll direction is `up`, `down`,
`left`, or `right`; `steps` defaults to 1 and ranges from 1 through 100.

Pointer input requires the current ready screenshot mapping. See
[Coordinates and focus](#coordinates-and-focus) for bounds and spatial risks.

### `keyboard`

Point focus left-clicks the PNG coordinate before sending the key action:

```json
{"state_id":"s-0000000000000001","focus":{"x":320,"y":180},"action":{"type":"press","key":"Ctrl+L"}}
```

Arrow keys accept `Left`, `Right`, `Up`, and `Down` as well as the common
`ArrowLeft`, `ArrowRight`, `ArrowUp`, and `ArrowDown` aliases.

Element focus is advertised only for elements with both the AT-SPI Component
interface and `focusable` state, and sends no pointer click:

```json
{"state_id":"s-0000000000000001","focus":{"element_id":"18"},"action":{"type":"type","text":"hello"}}
```

Both modes require a ready screenshot mapping and live generated input. Element
focus freshly relocates the object, calls `Component.GrabFocus`, and proceeds
only if the element reports focused and its exact window reports active. This
cannot prove compositor seat focus without a race. Do not type concurrently or
use focus-switch shortcuts. Chords containing both Alt and Tab return a
non-retryable `unsupported_action`; use an advertised focus target or
`launch_application` instead.

## Observation results

| Object | Fields |
| --- | --- |
| Observation | `state_id`, `target`, `view`, `element_query`, `screenshot`, `coordinate_spaces`, `elements` |
| `target` | `query`; `app` with `name`, `pid`, `object`; `window` with `title`, `object` |
| Object identity | `bus_name`, `path` |
| Screenshot | `ready`, `reason`, `width`, `height`, `coordinate_space` |
| Element | `element_id`, `depth`, `role`, `name`, `states`, `frame_atspi_window`, `inspection_complete`, `invoke`, `focus`, `named_actions`, `set_value` |

`set_value` is `"text"`, `"number"`, or `null`. `inspection_complete: false`
means action inspection failed. Do not infer an unreported invoke or named action;
use only capabilities reported positively. Focus and set-value support are
inspected independently and may still be available.

The text content reports each element's value or text when available, the first
non-empty selected text, and traversal-limit warnings. Those details are not
repeated in `structuredContent`.

When `screenshot.ready` is true, the result includes one complete-monitor PNG in
a separate `image/png` content block. It is not embedded in
`structuredContent`. `screenshot.width` and `screenshot.height` are its
dimensions after downscaling; the longest dimension is at most 1280 pixels.

The server prefers MCP `2025-11-25` and is tested with OpenCode. Negotiated
versions `2025-06-18`, `2025-11-25`, and SDK-supported `2026-07-28` receive
`outputSchema` declarations and `structuredContent`. Older versions receive
human-readable text and any PNG block without either structured field.

## State lifecycle

The runtime retains a count- and byte-bounded cache of up to eight observations,
with at most one latest state for each exact app and window.

| Event | Effect |
| --- | --- |
| Observe the same target | Replaces that target's prior state ID. |
| Observe another target | Keeps prior target states until replacement or eviction. |
| Begin an element, pointer, or keyboard mutation | Consumes the supplied state and clears screenshot mappings from every retained state. |
| Complete an action | Returns a replacement observation and state ID. |
| Launch an application | Clears the complete observation cache. |
| Cancellation after execution begins, or runtime cleanup | Clears every retained screenshot mapping. |
| Cancellation while queued | Leaves retained state unchanged because execution never began. |
| Cache eviction | Makes the evicted state ID stale. |

Clearing a screenshot mapping prevents later pointer or keyboard use of old
pixels. The acted-on state is always removed once mutation dispatch begins. A
semantic state for another retained target may remain usable for
`act_on_element` if fresh revalidation succeeds.

## Coordinates and focus

Use the dimensions in `screenshot`, not the physical monitor dimensions. Valid
points satisfy:

```text
0 <= x < width
0 <= y < height
```

Coordinates may be fractional. Negative values and points exactly on the right
or bottom edge are rejected. The server never clamps coordinates.

The state and stream metadata checks do not prove that the pixels are unchanged.
The user, another application, hover effects, animation, or a notification can
change what lies at a valid point after observation. Observe again when the
visual target may have moved or become occluded.

Element frames use `atspi_window_coordinates`. They are for semantic inspection
only and cannot be converted into PNG points. The server maps PNG fractions into
the compositor-private EIS region associated with the approved stream. If KDE
does not supply a `mapping_id`, the server requires the stream position and size
to match exactly one EIS region; otherwise observation can continue but
generated input is unavailable.

## Errors and outcomes

There are two MCP failure channels:

- An unknown tool or malformed `tools/call` envelope produces a JSON-RPC error.
- Invalid or unsupported actions for a known tool and runtime failures produce a normal
  response containing a tool result with `isError: true`. It includes text and,
  on supported versions, structured `code`, `message`, `outcome`, `retryable`,
  and `recovery` fields. Invalid and unsupported actions use outcome `not_started`;
  unsupported actions are non-retryable policy rejections.

Treat `outcome` as follows:

| Outcome | Meaning | Caller action |
| --- | --- | --- |
| `not_started` | The requested UI action was blocked before dispatch. | Follow `recovery`; usually observe and retry only if still needed. |
| `unknown` | The action may have started or completed. | Do not retry blindly. Observe and inspect the UI first. |
| `completed` | The action completed, but a later refresh or cleanup step failed. | Do not repeat automatically. Observe the current state. |

`retryable` is advisory. The `recovery` string describes the intended next
step. If generated input reports missing stream-to-EIS mapping, restart or
re-enable the MCP instead of repeating the same state.

## Direct call command

`computer-use-mcp call FILE` is a diagnostic batch interface, not a JSON-RPC
MCP transport. Use `-` for stdin. The input is one call object or a static array:

```json
[
  {"name":"list_applications","arguments":{"scope":"running"}},
  {"name":"observe","arguments":{"target":"KWrite"}}
]
```

The command uses the production runtime and may open the portal chooser. It
writes one MCP-style result per line. A runtime error writes its `isError: true`
result, stops the batch, reports to stderr, and exits nonzero. Invalid input also
exits nonzero, so automation must check status even when stdout has earlier
successful lines.

A static array cannot insert a `state_id` returned by an earlier entry. Use an
MCP host for observe-then-act workflows.

## Troubleshooting

```sh
computer-use-mcp doctor
computer-use-mcp list-apps
computer-use-mcp snapshot APP
computer-use-mcp init
opencode mcp list
```

`doctor`, `list-apps`, and `snapshot` do not request portal consent. `doctor`
checks prerequisites, not portal approval, capture, routing, or target access.
`init` has no deadline; cancel and retry it from the graphical session if the
portal stalls. MCP startup has a 60-second approval deadline.

| Symptom | Check or recovery |
| --- | --- |
| No target or ambiguous target | Use `list-apps`, then target a full app name, window title, or PID. |
| Portal chooser denied or timed out | Toggle `computer_use` in OpenCode's MCP UI or restart OpenCode, then approve exactly one monitor. |
| Established session revoked or stream lost | Toggle `computer_use` or restart the active OpenCode process; a tool retry cannot recreate the portal session. |
| `screenshot.ready: false` | Read `screenshot.reason`; semantic element actions may still work, but pointer and keyboard do not. |
| Tree limit warning | Increase `max_tree_nodes` or `max_tree_depth` within the documented maximum. |
| Stale or missing state | Observe again and use the new state ID. |
| Invalid coordinates | Use the returned PNG dimensions and keep both coordinates inside the half-open bounds. |
| Generated input unavailable | Read the tool error. Restart after grant or device changes; a missing stream-to-EIS mapping cannot be fixed by another observation. |

If `init` succeeds but startup later prompts again, KDE declined restoration or
the saved grant no longer matches the current display setup. The chooser remains
the authority.
