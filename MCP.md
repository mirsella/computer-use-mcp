# MCP guide

This document is the user-facing contract for `computer-use-mcp mcp`. See
[ARCHITECTURE.md](ARCHITECTURE.md) for implementation details and
[SECURITY.md](SECURITY.md) for the threat model.

## Trust boundary

Run this server only for a trusted local MCP host and user. AT-SPI may expose
private text and semantic side effects without a portal prompt. A screenshot is
the complete selected monitor, including unrelated windows and notifications.
Portal approval is session-level access, not approval for each later call.

Use an MCP host that lets the user inspect and deny sensitive calls. Review the
exact target and arguments before observations, launches, activation, and
mutations.

## Transport and startup

The transport is stdio only:

```text
computer-use-mcp mcp
```

The host owns stdin and stdout. Do not run `mcp` interactively or wrap it with
anything that writes to stdout. JSON-RPC messages use stdout; diagnostics use
stderr.

The protocol starts immediately while KDE portal approval and PipeWire setup
run in the background. `list_desktop`, `launch_application`, and
accessibility-only observations do not wait for capture. Denial, timeout,
revocation, or stream loss exhausts portal-backed functionality for that
process. Restart the MCP to try again.

### OpenCode configuration

```sh
opencode mcp add computer_use -- "$(command -v computer-use-mcp)" mcp
```

Require review for every tool:

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

OpenCode applies the last matching permission rule. Put
`"computer_use_*": "ask"` after any broader wildcard rule.

### Portal restore token

`computer-use-mcp init` opens and closes a temporary approval session and
requires KDE to return a reusable token. The private one-shot token is stored at
`$XDG_STATE_HOME/computer-use-mcp/portal-restore-token`, or
`~/.local/state/computer-use-mcp/portal-restore-token` when `XDG_STATE_HOME` is
unset. Startup claims and removes it before use, then stores only the replacement
returned by a successful portal start.

Set `COMPUTER_USE_MCP_PERSIST_PORTAL=0` before MCP startup to skip token loading
and storage for that run; it neither deletes an existing token nor affects
`computer-use-mcp init`, which always requests persistence.

## Initialize context and request rules

The server exposes exactly six tools: `list_desktop`, `launch_application`,
`activate_window`, `observe`, `act`, and `wait_for`. Every input object and
nested variant is closed; unknown fields are rejected. The initialize response
contains a compact operational contract and each `tools/list` entry contains
call-local guidance, so no progressive help call is needed. The server does not
advertise prompts, MCP resources, or resource templates. An optional native
skill is available at `.agents/skills/computer-use-mcp/SKILL.md` and is kept
byte-identical to the packaged `guidance/skill.md` artifact. Tools do not
advertise `outputSchema`; structured content is returned only when the
negotiated protocol supports it.

- Call `list_desktop` first and copy one complete opaque target exactly.
- Never substitute a title, app name, PID, traversal index, geometry, or fuzzy
  selector for an opaque ID.
- `observation_id`, `frame_id`, and `element_id` are scoped to the returned
  observation or process lifetime. Copy them unchanged. `act.source_observation.frame_id`
  may be omitted only for semantic operations; pointer and keyboard operations
  require the exact source frame. Keyboard focus is always a point in that same
  ready screenshot PNG; an AT-SPI focused element never authorizes EIS keyboard.
- Use only capabilities advertised in the same observation.
- Treat stale, unknown, completed, unavailable, and timeout results as stop
  signals. Observe again instead of retrying blindly.
- For keyboard, click the visibly intended input field or address bar from the
  exact source PNG; never click an arbitrary page and route focus with a
  shortcut. Prefer semantic `set_value` when advertised.

## Tools

### `list_desktop`

```json
{"scope":"windows"}
```

Results are paged. `limit` defaults to 50 and is bounded at 100; pass the
returned opaque `next_cursor` as `cursor` for the next page. Do not reuse a
cursor after the desktop catalog changes. Human-readable page text and
`structuredContent` are each capped at 16,000 UTF-8/serialized-JSON bytes;
bounded projections identify omitted entries and must be treated as incomplete.

Returns process-lifetime `app_instance_id` and `window_instance_id` values,
descriptive metadata, backend authority, and per-target capability states. The
`windows` result also includes a `backends` object with explicit
`supported`/`unsupported`/`unavailable`/`busy` status for the standard and KDE
compositor authorities. Compact text includes those backend statuses and each
window's source plus screenshot/accessibility/activation capability status;
`structuredContent.windows` remains canonical for geometry and full reasons.
The other scope lists exact installed `.desktop` IDs:

```json
{"scope":"applications"}
```

The standard foreign-toplevel authority supplies identifier/title/app ID only;
PID, geometry, outputs, accessibility, and activation remain unavailable. The
optional KDE-rich authority requires `COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT=1`
and a Plasma window-management global version 17 or newer. It is a compositor
single-client protocol and reports Busy/Unavailable rather than weakening the
AT-SPI or capture path when binding is not possible. KDE geometry and virtual
desktop fields are logical diagnostics only and are never used to crop PNGs or
map input coordinates.

### `launch_application`

```json
{"desktop_id":"org.kde.kwrite.desktop"}
```

The ID must be copied exactly from `list_desktop(applications)`. No command,
path, or arguments are accepted. The result is a launch request, not proof that
the application has mapped a window; list the desktop again before targeting it.

### `activate_window`

```json
{
  "target": {
    "app_instance_id":"app-0000000000000001",
    "window_instance_id":"win-0000000000000002"
  }
}
```

Activation reports request acceptance/flushing separately from authority-specific
evidence. AT-SPI uses `status: "atspi_active_observed"` after a fresh active
state read and never claims compositor verification or seat focus. KDE rich
activation may use `status: "protocol_state_verified"` only for a matching
post-request active transition; an already-active KDE target reports no
transition proof. `dispatch.synchronized` is always `false`: server
synchronization, seat focus, and client delivery are not observable.

### `observe`

```json
{
  "target": {
    "app_instance_id":"app-0000000000000001",
    "window_instance_id":"win-0000000000000002"
  },
  "view":"both",
  "accessibility": {
    "scope":"interactive",
    "query":"save",
    "limits":{"text_limit":500,"max_nodes":1200,"max_depth":64}
  }
}
```

`view` is `screenshot`, `accessibility`, or `both`. Accessibility scopes are
`full`, `visible`, and `interactive`; the latter two prune the returned tree.
Accessibility-only calls do not wait for portal capture. Screenshot coordinates
are PNG half-open pixels: `0 <= x < width` and `0 <= y < height`.

The result includes an opaque `observation_id`, target, readiness diagnostics,
PNG/frame metadata, coordinate-space declarations, and opaque element IDs.
For a ready screenshot, the human-readable output includes the exact
`PNG frame_id` that must be copied for pointer or keyboard actions. It also
reports frame timestamp authority, source-sequence/PTS availability, and stream
health in compact text. Action results retain compact request,
flush, synchronization, effect, and seat-focus evidence alongside their
structured replacement observation.

Accessibility observation text and structured metadata are each capped at
16,000 UTF-8/serialized-JSON bytes. If the requested tree cannot fit, complete elements are
kept where possible (including the root and focused element), text fields are
Unicode-safe truncated, and `truncated`, `truncation_reason`, and element
counts identify the response budget. JSON is never cut at a serialized byte
boundary. Action replacement observations use the same evidence.
The `e-...` ID on each human-readable element line is the same value as that
element's structured `element_id`; copy it unchanged for semantic actions and
element waits.
Accessibility bounds are diagnostic only and must not be converted into PNG
coordinates. A ready screenshot is returned as a separate `image/png` content
block.

### `act`

Every `act` call names the exact source observation:

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
  "operation": {
    "type":"semantic",
    "element_id":"e-0000000000000005",
    "action":{"type":"invoke"}
  }
}
```

Operation types are:

- `semantic`: `invoke`, `focus`, exact advertised `named`, or `set_value`.
- `pointer`: `move`, `click`, bounded multi-point `drag`, or directional
  `scroll`; coordinates must use the exact source PNG.
- `keyboard`: explicit point focus in the exact source PNG and a bounded
  transaction that is either 1–8 press-only events (at most four modifiers per
  chord) or exactly one non-empty `type` event of at most 4,096 Unicode scalar
  values. Mixed `press`/`type` events, NUL text, and empty text are rejected.
  The point click is requested, sent, flushed, and protocol-synchronized before
  key events in the same cleanup-safe EIS transaction; phase barriers between
  high-level presses are protocol synchronization only. Element focus is
  semantic-only and cannot authorize keys. Alt+Tab is unsupported; use a
  visibly intended point, separate actions with replacement observations
  between routing/text/submit, or an advertised semantic `set_value`.

Semantic actions may work without a frame. Pointer and keyboard actions require
the exact ready source frame and a current stream mapping. Results expose
`dispatch.request_accepted`, `dispatch.protocol_request_sent`, and
`dispatch.request_flushed`, distinguish those facts from replacement-observation
evidence, and never claim an application effect, seat focus, application
delivery, or text delivery. Keyboard action evidence explicitly reports
`focus.click.requested`, `focus.click.sent`, `focus.click.flushed`,
`seat_focus: "not_observable"`, `application_delivery: "not_observable"`, and
`text_delivery: "not_observable"`. The exact source frame age, PNG bounds,
stream mapping, accessibility generation, portal-session identity, and stream
health are checked. Pre-dispatch validation reuses that exact source mapping and
does not wait for a strictly newer complete frame; a newer committed frame may
validate unchanged format/crop/transform invariants but never remaps source
coordinates. A whole-frame `change_epoch` requirement is intentionally not
imposed because animated full-monitor content creates an unavoidable visual race.

When an `act` reaches action-attempt tracking, its result includes
`action_progress`. Its `dispatch_stage` is `not_started`, `started`, or
`completed`; `cleanup` is `not_needed`, `completed`, or `failed`; and
`post_visual` and `post_accessibility` distinguish an observed replacement from
timeout, stream degradation, session loss, catalog failure, accessibility
failure, or an unavailable authority. Pre-dispatch validation and
`not_started` failures before tracking may omit progress. Once tracking begins,
these fields are attempt evidence only: they never claim application delivery
or effect.

### `wait_for`

```json
{
  "target": {
    "app_instance_id":"app-0000000000000001",
    "window_instance_id":"win-0000000000000002"
  },
  "condition": {
    "type":"frame_changed",
    "after_frame_id":"frame-0000000000000004"
  },
  "timeout_ms":3000
}
```

Conditions cover later/changed/stable frames, later accessibility content, and
specific element state or value. The bounded result says whether the condition
was satisfied and includes frame or observation evidence. A timeout is not proof
that nothing changed outside the bounded observation authority.

## Errors and outcomes

An unknown tool or malformed JSON-RPC envelope produces a JSON-RPC error.
Invalid arguments and runtime failures for a known tool produce a normal tool
result with `isError: true` and structured fields:

| Outcome | Meaning | Caller action |
| --- | --- | --- |
| `not_started` | Dispatch was blocked before the action. | Follow `recovery`; observe before retrying if needed. |
| `unknown` | Dispatch may have started or completed. | Observe current state; do not retry blindly. |
| `completed` | Dispatch completed but later evidence or cleanup failed. | Observe current state; do not repeat automatically. |

`retryable` is advisory. The recovery text is authoritative for the next safe
step. Missing stream-to-EIS mapping, stream loss, or portal exhaustion requires
restarting or re-enabling the MCP.

Pre-dispatch validation errors and failures before action-attempt tracking may
omit `action_progress`; if it is absent, no action-attempt stage was entered.
Once tracking begins, both successful and failed `act` results carry it.

## Direct commands and troubleshooting

```sh
computer-use-mcp doctor
computer-use-mcp init
opencode mcp list
```

`doctor` reports prerequisites without requesting portal consent. `init` is the
explicit KDE approval flow. `call FILE` is a diagnostic batch interface and
uses the same six tool names; it cannot insert an observation ID returned by an
earlier static array entry.

| Symptom | Recovery |
| --- | --- |
| Target unavailable or stale | Call `list_desktop(windows)` again and copy a new target. |
| Capture denied or timed out | Restart/re-enable the MCP and approve exactly one monitor. |
| `screenshot.ready` is false | Read its reason; semantic observation/action may still work. |
| Missing capability | Use only a capability advertised by the exact target. |
| Invalid coordinates | Use returned PNG dimensions and half-open bounds. |
| Unknown/completed action | Observe current state before deciding whether another action is needed. |
