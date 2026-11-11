# MCP Guide

This is the caller-facing guide for `computer-use-mcp mcp`. The server exposes
six operations: `list_desktop`, `launch_application`, `activate_window`,
`observe`, `act`, and `wait_for`. Plain `mcp` exposes them as direct tools.
`mcp --compact-tools` exposes only `help` and `dispatch` over the same broker.

- `help {}` lists operation names. `help {"action":"observe"}` returns that
  operation's description and a compact reference in one text block, without
  initializing a desktop: copyable argument templates plus every field, enum
  value, and limit. Each template passes the native validator, and a test
  keeps the reference complete against the input schema. The full JSON Schema
  stays on the direct `mcp` tool definitions.
- `dispatch {"action":"observe","arguments":{...}}` executes the operation
  using normal validation, identity routing, cancellation, and result encoding.
  Direct tool names are unavailable in compact mode; recursive dispatch and
  unknown operations are rejected before desktop startup.

The compact dispatch description includes validated argument templates for
window discovery, accessibility observation, and semantic invoke/set-value.
These common paths need no help call. Fetch the help reference for other
fields and operations, including screenshot-grounded pointer and keyboard input.
This is the same native validator, not a second permissive action interface.

Annotations mark `list_desktop`, `observe`, and `wait_for` read-only and
idempotent. The first operation needing the accessibility tree may enable the
session's AT-SPI bridge (see [SECURITY.md](SECURITY.md)); that one-time service
activation changes no desktop state.

Initialize carries shared evidence/recovery rules. Direct tool definitions or
on-demand help carry operation details; the skill carries workflow guidance.

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

The npm package includes the precompiled Linux x64 server. With OpenCode 1.18
or newer, add `"@mirsella/computer-use-mcp"` to the `plugin` array and
restart OpenCode. The plugin registers compact tools and the bundled skill.
Per-tool `permission` or `tools` rules at global or agent scope select direct
tools automatically; broad `computer_use_*` rules apply in either mode.
The plugin option `compactTools: false` also selects direct tools. Explicit
`mcp.computer_use` settings take precedence over plugin defaults. Manually
configured compact servers require dispatcher-wide permissions.

Alternatively, register the package as a stdio MCP server with a `90000`
millisecond timeout. Choose permissions for your host; this example asks before
each call:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "computer_use": {
      "type": "local",
      "command": ["npx", "-y", "@mirsella/computer-use-mcp", "mcp"],
      "enabled": true,
      "timeout": 90000
    }
  },
  "permission": { "computer_use_*": "ask" }
}
```

Source installations can use `["/absolute/path/to/computer-use-mcp", "mcp"]`
instead. OpenCode applies the last matching permission rule. Check registration with
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
`frame_stable` measures unchanged pixels on a healthy capture stream, including
quiet periods on damage-driven compositors. It retains the existing frame ID
when no new image arrives; it does not claim that a fresh frame was captured.
A failed, missing, or renegotiated stream cannot establish stability. A complete
frame with a prior sequence gap can establish a new quiet interval; its degraded
continuity metadata is retained.

The private runner enables KDE window-management metadata by default so its
owned windows can expose geometry and window controls. The owned compositor
runs with `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1` to expose these interfaces without
installed desktop-file grants; this setting is scoped to that compositor.
An explicit
`COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT=0` still disables that backend.

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
  to a process, observation, or source frame. An `element_id` stays bound to the
  same accessibility object across consecutive observations of the same window,
  so the latest readback for that window remains the authoritative source; every
  semantic action still re-reads the exact object before dispatching. A newer
  observation of the same window replaces only the authority it carries: an
  earlier observation stays a valid element source while it lists elements that
  no newer retained observation of that window lists, so a screenshot-only look
  or a narrow search does not strand IDs from a wider observation, and an
  accessibility-only observation keeps the latest unmutated frame. Retention
  grants identity only; any action on the window still invalidates its frames.
- A bare `app_instance_id` plus `window_instance_id` beside the other arguments
  is read as `target`; a bare `observation_id`/`frame_id` beside `operation`, or
  a `source_observation` inside it, is read as `act.source_observation`. A
  semantic action type used as `operation.type` together with an `element_id`
  is read as the semantic operation, and `named_action` as the action `name`. These
  are spelling tolerances only: every ID is validated exactly as usual.
- Key chords are validated with the arguments. Names accept the documented
  aliases plus X keysym names such as `comma`, `period`, `plus`, or `Menu`,
  case-insensitively; a chord has at most one non-modifier key.
- Use only capabilities advertised for the exact target and observation.
- Treat `unknown`, `completed`, stale, unavailable, and timeout results as
  evidence to inspect, not as permission to retry blindly.

Semantic operations may omit a frame. Pointer operations and point-focused
keyboard or paste operations require the exact ready PNG frame named by
`act.source_observation.frame_id`. A semantic focus operation uses an
`element_id`, requests AT-SPI focus if needed, and verifies focus and active-window state
without moving the pointer.

## Tools

### `list_desktop`

```json
{"scope":"windows"}
```

Use `{"scope":"windows","desktop":"background"}` to discover the
private background desktop. `scope` defaults to `windows` when omitted.
`desktop` is optional and defaults to foreground;
the applications scope accepts the same explicit selector.

Results are paged. `limit` defaults to 50 and is bounded at 100. Pass the
returned opaque `next_cursor` unchanged. Do not reuse a cursor after catalog
membership changes.

The windows result includes process-lifetime `app_instance_id` and
`window_instance_id` values, descriptive metadata, backend authority, and
capability states. Standard foreign-toplevel inventory supplies title and app
ID but not PID, geometry, accessibility, or activation. The optional KDE-rich
authority requires `COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT=1` (the private
runner's default) and a Plasma
window-management global version 17 or newer. It can be unavailable or busy
because it is a single-client protocol; that does not weaken other authorities.

The result reports backend status as `supported`, `unsupported`, `unavailable`,
or `busy`. KDE geometry and desktop fields are logical diagnostics. AT-SPI
extents are not input or crop authority.

An exact direct child of an AT-SPI application may be a visible Component
`filler` or `panel` rather than a frame (as in the tested Qt QWidget). It can
provide an accessibility target under its own native object identity. This is
not a compositor-window match, and grants no geometry or pixel authority.

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
AT-SPI can report a fresh active-state observation. KDE verifies an active
state event or the exact window's active state after a request-ordered compositor
sync, which also covers an app that activates itself just before the request. Neither proves seat focus or application
delivery. `dispatch.synchronized` reports server synchronization only.

KDE activation may switch one verified KWin desktop before dispatch. An
unknown desktop location does not become proof through a guess. A bounded
AT-SPI desktop hunt may try each non-current desktop once and restore the
original desktop when no active state verifies.

Toolkits commonly decline AT-SPI activation, because a Wayland client cannot
raise its own window. When the toolkit declines and one fresh read shows the
window still inactive, the call ends with `capability_unavailable` and outcome
`completed`: nothing is uncertain and no desktop hunt runs. Activate the
`kde-plasma-window-management` entry `list_desktop` shows for that window, or
click inside it from a fresh screenshot. The server does not pair the two
entries itself, since title and PID do not identify a window.

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
  "accessibility":{"scope":"interactive","limits":{"max_nodes":40}}
}
```

`view` is `screenshot`, `accessibility`, or `both`. Accessibility options are
`scope` (`full`, `visible`, or `interactive`), `query` or `search`, and a `limits` object
with `text_limit` (characters or `"max"`), `max_nodes`, and `max_depth`.
Accessibility-only observations do not wait for portal capture. Unknown
argument names are rejected.

`query` remains a substring filter on already-collected metadata. For a bounded
native search instead, use, for example:

```json
"accessibility": {
  "scope": "interactive",
  "search": {"name": "Save", "role": "push button", "exact": true, "limit": 5}
}
```

Search matches native accessible **name** and/or **role**, case-insensitively;
both predicates must match when supplied. Role is always an exact native role
name; `exact` changes name matching from substring to equality. Exact misses
never loosen. This is not a value/text-content search or a persistent selector.
`query` and `search` cannot be combined. Name/role are nonblank strings of at
most 1,000 characters. Search defaults: `limit=20` (1–100), `max_nodes=500`
(1–5,000 attempted discovery reads), `timeout_ms=2000` (1–2,000). The ordinary
output-node, depth, scope, text and response-byte limits still apply.

Traversal prioritizes focused and showing controls outside collections before
large collection contents, then emits selected nodes in original tree order.
Ordinary discovery attempts have a separate cap of
`clamp(4 * limits.max_nodes, 64, 10000)`. Discovery reads native name, role,
states and bounded child edges; action, text/value and geometry metadata is
acquired only for retained nodes. Uninspected capabilities are not advertised
as supported. These choices do not make AT-SPI geometry input authority or
weaken fresh exact-object action revalidation.

Visible/interactive scope keeps selectable page tabs for navigation but omits
descendants of tabs whose native selected state is false. Full scope retains
them. A search that skips such contents reports `scope_omitted`; it cannot prove
absence in another page. Noninvokable tabs are identified explicitly in text;
use a supported parent tab-list keyboard operation or screenshot input instead
of inventing an `invoke` action.

Visible/interactive traversal also skips closed, nonshowing collections whose
descendants can otherwise claim misleading visible states. Short navigation
lists are visited before deep settings pages and large inventories. Empty
focus-only containers are not presented as interactive controls. Full scope
retains these objects; skipped search scope cannot establish absence.

`accessibility.traversal` reports `visited_nodes`, `detailed_nodes`, `truncated`
and `stop_reason`. Incompleteness can result from node/result, discovery, depth,
time, scope or read-error bounds. Reaching a search result limit conservatively
reports incomplete. **An incomplete empty search does not prove absence.**
Results carry the usual observation-bound opaque element IDs. Search is not
run for screenshot-only observations. A separate top-level `truncated` flag
reports response projection limits, not search completeness.

The optional crop is `monitor` (default) or `target_window`. Target-window crop
uses KDE logical geometry, never AT-SPI extents. It is applied to raw source
pixels before transform, downscale, and PNG encoding. The result retains the
exact mapping back to the approved EIS region. Missing or untrusted geometry
falls back to the full monitor with an explicit reason.

Screenshot coordinates are half-open PNG pixels. A ready result contains the
exact `frame_id` required for spatial input. Accessibility elements expose
opaque `element_id` values. Text and structured output are bounded and report
truncation without cutting serialized JSON at an arbitrary byte.

The text projection omits repeated action descriptions and duplicate noneditable
name/value strings. Named action identifiers remain available; the structured
projection retains full metadata. Checkable controls report `checked=true/false`
and decision-relevant states such as selected, expanded, or invalid. An
unfiltered interactive observation also includes up to eight visible label/status
names within a 1,024-byte escaped-JSON budget. Text identifies these as fresh
AT-SPI readback, not dispatch success. Omitted nodes do not invalidate included
facts; another observation is needed only if the included evidence does not
establish the requested result. This does not attribute causality to the action
or prove anything about omitted nodes.

Actions may include `keybinding` copied from native AT-SPI metadata. It is
bounded, locale/toolkit-dependent text, not a portable command or input
authority. Some toolkits use `mnemonic;sequence;shortcut`; others return a
single mnemonic. An absent binding must not be guessed. Modal-opening actions
can block AT-SPI `DoAction`; a fresh screenshot-grounded click avoids that
synchronous reply path. No automatic action replay or shortcut fallback occurs.

Ready screenshots may include `changed_rect`, an advisory tile-hash bounding box
against the previous committed captured frame. It is not a model observation
delta. The first captured frame has no baseline. A new stream may still return
that complete baseline if no newer frame arrives during startup capture.

### `act`

For a semantic operation or keyboard/paste with semantic focus, the exact live
`element_id` identifies the newest retained observation that contains it. Both
`source_observation` and `target` may then be
omitted. The retained source still binds one exact application/window target
and desktop route. Supplied source/target authority must match; stale or
unbound authority fails before input. Omission never selects the current
foreground window or infers identity by name/PID/geometry. Pointer and
point-focused keyboard/paste always require explicit source and frame.

Keyboard and paste operations may omit `focus`. Input then goes to whatever
holds keyboard focus inside the exact `target` window, which suits shortcuts and
typing straight after a click. With `target` or a `source_observation` it uses
the newest retained observation of that window; with neither it binds the
window this session last observed or acted on. It moves neither the pointer
nor AT-SPI focus. It is refused before any input with
`target_unavailable` unless a fresh read shows the window active or a click
focus grace applies, and the active state is re-read after input preparation.
It never types into another window; `activate_window` first if needed.

```json
{
  "operation":{"type":"semantic","element_id":"e-0000000000000005","action":{"type":"invoke"}}
}
```

Semantic operations include `invoke`, `focus`, advertised `named` actions, and
`set_value`. When a control exposes both numeric Value and EditableText, the
native numeric property is preferred to avoid leaving uncommitted spin-box text.
Pointer operations require that source's `frame_id` and use exact
source-PNG coordinates. Keyboard
operations contain 1 to 8 ordered `press` or `type` events, with at most 4,096
Unicode scalar values across all typed text. For example, text followed by
Enter may share one transaction. Every event is resolved before emission;
invalid later input cannot leave an earlier event dispatched. Point focus,
ordered key events, synchronization, and cleanup stay in one EIS transaction.
The click is sent and flushed before keys are dispatched; high-level events
have protocol barriers between them. This does not establish application-level
completion between events. Keep navigation requiring new UI evidence separate.

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
Semantic typing requires fresh exact focused-element and active-window
verification after EIS preparation. Already verified focus avoids a redundant
`Component.GrabFocus`; this matters for providers such as the tested GTK4 bridge
that can expose valid editing while rejecting focus requests. Otherwise focus
is requested and reverified. A false or failed request is accepted only when
that fresh exact verification succeeds. Text replacement similarly skips a
redundant focus request only after a fresh focused/non-defunct read, and still
requires exact full-text readback.

Keyboard keymap/device resolution precedes any semantic focus mutation,
not just key emission. Typed text prefers physical keys; otherwise it can use
negotiated EIS Text keysym events on the same exact seat. Both the physical
keyboard and Text device must retain their exact object and resume generation.
Shortcuts remain physical-keymap-only. Text UTF-8/input-method insertion is
never substituted for key events. Unsupported keysyms detected in preparation return
`unsupported_input`, `not_started`, non-retryable for the same typing. A later
device/keymap change still follows the actual mutation-progress outcome rules.
Portal paste is an alternative
only when negotiated and the application accepts paste; it is not a general
Unicode key-event synthesis mechanism for a custom drawing canvas.

Action results separate dispatch, cleanup, and post-visual or
post-accessibility evidence. They do not prove text delivery, application
effect, seat focus, or client delivery.

Actions return fresh post-action state when available. Use its replacement
observation, element IDs, and target for the next action; inspect that state
before requesting another observation. A freshly active same-application modal,
or the sole exact surviving window after a modal disappears, can become the
**read-only replacement observation target**. The result explicitly returns
`replacement_target` and the full replacement observation. Selection requires
the same opaque application identity and exact AT-SPI app object/PID; it never
joins by title/PID alone, crosses compositor identities, or retargets input.
Ambiguous survivors remain a refresh error. An already-active sibling must not
steal the source window's replacement state. A replacement is not evidence that
the preceding action caused the transition.

A successful action's text readback lists only elements that are new or whose
rendered line differs from what this session last showed for that element ID,
plus the acted-on element. One line counts the omitted unchanged elements, whose
IDs remain valid, and another describes by role and name, without IDs, the
previously shown elements that are no longer listed. `observe` and `wait_for` always return the full requested tree,
`list_desktop` resets the comparison, structured content is never reduced, and
readbacks attached to uncertain-outcome errors stay complete.

When an action leaves a new window of the same application that is not the
replacement target, such as a context menu or a non-modal dialog, the result
appends up to two `Opened window` blocks. Each gives that window's exact target
and up to 24 interactive elements, and `opened_windows` in structured content
carries the target, title, and observation ID. Elements listed there act on
that window's target, never on the source window. The blocks are charged
against the same response budget: a cut list ends with `(more elements; observe
it)`, and a window that does not fit is only announced. A reported window
counts as the window last observed, so keyboard input that names no window
goes to it while it is active. Untitled windows without
interactive elements, such as tooltips, are not reported. This is read-only
evidence and does not prove the action caused the window.

The readback follows the action when the source window is gone: after input
closes a dialog, it describes the same application's single active or single
remaining window and names that target, for screenshot-sourced input as well
as semantic actions. Ambiguous survivors are never chosen.

When this changes an interactive observation's exact target, its automatic
readback is capped at 24 detailed nodes and clears old-window search filters.
This keeps dialog navigation concise, not complete. Truncation remains explicit;
request a scoped search or larger explicit observation if the needed control is
missing. The cap does not apply to full-scope or same-target readbacks.

### `wait_for`

`target` is optional for window conditions and forbidden for `human_idle`.
`condition` and `timeout_ms` are required. The timeout is bounded at 5000
milliseconds, except `human_idle`, which allows up to 120000 milliseconds. Frame,
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

An unsatisfied result distinguishes `catalog_refresh_timeout`,
`app_identity_unavailable`, and `window_not_observed` in `evidence.reason`.
Unsupported compositor identity backends return immediately; temporary
unavailability can recover during the wait. The first two report unavailable
evidence rather than claiming no change. Recovery directs callers to list
windows on the same desktop and observe an exact returned target. None of these
results establishes that launch failed.

`element_value` matches the complete accessibility value, or complete text when
no value interface is present. Truncated text cannot establish an exact match.

Window close:

```json
{
  "condition":{"type":"window_closed","window_instance_id":"win-0000000000000002"},
  "timeout_ms":3000
}
```

The wait refreshes the authoritative catalog and requires an exact opaque ID
observed in that catalog. The most recent 128 retired identities are retained
so a window that closes before the wait begins can still be verified. These
identities are retained only when the source backend actually enumerated the
window's disappearance. A backend outage or a switch to another catalog source
invalidates continuity without proving closure. These identities cannot authorize
input. A supplied `target` must match both the
application and window identity. An unknown or evicted ID, or an unavailable
source backend, is not treated as closure.

Window waits do not subscribe to an event channel. They refresh and poll until
the bounded timeout or an explicit catalog error. A timeout is not proof that
nothing changed outside the catalog authority.

Human idle:

```json
{
  "condition":{"type":"human_idle"},
  "timeout_ms":120000
}
```

This targetless, foreground-only condition does not accept `desktop`. It waits
until 60 seconds have passed since the last detected physical activity, all
observed physical keys and buttons are released, and no cooperative handoff
signal is asserted. New activity restarts the quiet period. Clearing a handoff
signal starts a fresh quiet period. A newly started monitor with no detected
activity has no pending cooldown.

The result reports `satisfied`, `evidence.available`, `quiet_period_ms`,
`remaining_ms`, and a reason: `idle`, `quiet_period`, `physical_input_held`,
`handoff_requested`, or `physical_monitor_unavailable`. Remaining time describes
the cooldown, not a prediction of when held input or a handoff will clear.
Unavailable physical monitoring never establishes idle. Detection remains best
effort for devices the process cannot read.

An idle wait tolerates ongoing human activity and supports normal cancellation.
It neither dispatches nor replays an action. After success, obtain a fresh
observation before deciding whether to act.

## Outcomes and Takeover

Known-tool argument and runtime failures are normal tool results with
`isError: true` and structured outcome fields:

| Outcome | Meaning | Next step |
| --- | --- | --- |
| `not_started` | Dispatch was blocked before the action. | Follow `recovery`; observe before retrying if needed. |
| `unknown` | Dispatch may have started or completed. | Observe current state; do not retry blindly. |
| `completed` | Dispatch completed but later evidence or cleanup is incomplete. | Observe current state before acting again. |

An unknown semantic-action or semantic-focus error can include a fresh
`replacement_observation`. This is read-only recovery evidence, not proof that
the failed call completed. Its target may be a newly opened modal in the exact
same native application. Use its new IDs when explicitly continuing; never
replay the failed action merely because readback exists. Truncated evidence
still requires an appropriately scoped observation before selecting a control.

`retryable` is advisory. The recovery text is the next-step authority.
`ProtectedSurfaceRefused` is non-retryable.

Physical monitoring runs for the lifetime of the foreground worker, including
between calls. It uses
readable physical `/dev/input/event*` devices, the cooperative
`COMPUTER_USE_MCP_TAKEOVER=1` flag or handoff file, and EIS refusal caused by
physical shortcut modifiers. Agent EIS events do not appear as physical device
events. Monitoring is skipped only for a verified isolated session.

Foreground mutations fail immediately with `HumanInputBusy` while input is
active or recent. Detection during a mutation interrupts further dispatch and
attempts held-input cleanup and desktop restoration. The interrupted operation
stays aborted. Outcomes retain actual dispatch progress; cleanup cannot retract
delivered input. Read-only discovery, observation, and waits remain available.

After the 60-second quiet period, a new mutation may proceed without restarting
MCP. Observations from before or during the pause become stale. Use
`wait_for human_idle`, then observe again, or hand control to the user. Background
use requires the user's request; human activity never changes the desktop route.

Human activity also invalidates retained accessibility and frame evidence.
An observation-dependent wait can therefore return `stale_state` immediately
after an observation if the user continues interacting. The error identifies
human input as the cause. Read-only observation remains available; foreground
actions must wait for `human_idle` and then observe again.
Cleanup failure is a separate session failure and cannot be repaired by an idle
wait. Detection is best effort when no physical device is readable and no
cooperative signal exists.

## Direct Commands and Troubleshooting

```sh
computer-use-mcp doctor
computer-use-mcp history
computer-use-mcp history --errors
computer-use-mcp history --last 50 --since 2h
computer-use-mcp history --call-id connection-0123456789abcdef-4
computer-use-mcp init
opencode mcp list
```

`doctor` reports prerequisites, display binding, isolation verdict, watcher
state, and portal readiness without requesting consent. `init` requests portal
approval. `call FILE` runs production validation and runtime against a static
batch; static entries cannot feed returned opaque IDs into later entries.

### Call history

The public broker records every tool call, including compact `help`/`dispatch`,
validation failures, cancellations, and protocol-level unknown-tool errors.
The CLI `call` command uses the same history. Each call has a start record and
a terminal record with a connection/call ID, timestamp, elapsed milliseconds,
operation, validated request metadata, desktop route when resolved, result
code, outcome, retryability, and action-progress fields when available.
Successful protocol completion does not prove application effect.

`history` prints retained JSONL records in append order. `history --errors`
prints failed and abandoned calls. A dropped execution future is recorded as
abandoned with an unknown outcome; a killed process may leave only its start
record. Connection and call IDs distinguish simultaneous MCP instances.

Filters combine in any order. `--since` selects calls by their start timestamp;
it accepts Unix milliseconds or an integer duration ending in `s`, `m`, `h`, or
`d`, relative to the current time. `--call-id` matches an exact ID. `--last N`
selects the most recently appended N distinct matching calls after the other
filters, preserving their retained records in append order. It counts calls,
not JSONL lines, even when concurrent calls interleave. `--last 0` prints nothing.
Combined with `--errors`, it returns only the matching terminal error records.

History lives in `$XDG_STATE_HOME/computer-use-mcp/history`, or
`~/.local/state/computer-use-mcp/history` when XDG state is unset. Two rotating
files, `calls.jsonl` and `calls.previous.jsonl`, retain at most 1 MiB each.
Older records are removed on rotation. A shared file lock serializes readers
and writers across processes. The directory is private (0700), and new files
are private (0600). History survives ordinary MCP restarts.

Records contain metadata and text lengths, not typed or assigned values,
clipboard contents, accessibility text, screenshots, raw arguments, or error
messages. Logging failures are reported on stderr and do not change dispatch
or retry semantics. `doctor` reports the history directory.

| Symptom | Recovery |
| --- | --- |
| Target is stale or unavailable | Call `list_desktop` with `scope: "windows"` and copy a fresh target. |
| A worker is retired after capture denial, revocation, or exhausted approval | Approve the intended monitor if requested, then call explicit `list_desktop` or `launch_application` to create a replacement worker. |
| Screenshot is not ready | Read its reason; an accessibility-only observation may still work. |
| Capability is unavailable or busy | Use a capability advertised by the exact target, or wait for the authority to recover. |
| Coordinates are invalid | Use the returned PNG dimensions and half-open bounds from the exact source frame. |
| Outcome is `unknown` or `completed` | Observe current state before deciding whether another action is needed. |
| `HumanInputBusy` | Pause foreground actions. Wait for `human_idle`, then observe fresh state; no restart is needed. Clear an asserted handoff signal before resuming. |
