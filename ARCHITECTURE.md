# Architecture

[MCP.md](MCP.md) is the public behavior reference. [SECURITY.md](SECURITY.md)
defines the trust boundary. This document records the implementation boundaries
and the authorities used to make decisions.

## Boundaries

- `contract` and `validation` define the ordered tool schemas and convert
  untrusted JSON into typed calls. The optional compact transport fetches these
  same schemas on demand and unwraps dispatch before ordinary broker validation;
  it owns no separate execution or desktop state.
- `broker`, `server`, `cli`, `runtime`, and `errors` own MCP transport,
  per-desktop scheduling, presentation, and outcome reporting. The broker owns
  one lazy foreground worker and one lazy background worker; their local IDs are
  translated to collision-free public IDs and route later calls automatically.
- `accessibility` resolves targets, reads bounded AT-SPI trees, caches
  observations, performs semantic actions, and applies protected-surface and
  focus policy. `atspi_adapter` is the production AT-SPI implementation.
- `window_backend` reconciles exact backend records into opaque process-lifetime
  application and window IDs. `wayland_catalog` runs blocking Wayland I/O on
  dedicated threads and owns foreign-toplevel inventory plus optional KDE
  activation and window-state protocol calls.
- `desktop_launcher` lists installed GIO applications and launches one exact
  `.desktop` ID.
- `portal` owns RemoteDesktop and ScreenCast requests, grants, sessions, and
  restore tokens. `capture` owns the PipeWire stream. `geometry`, `encoder`,
  and `screenshot` validate frames and retain their source mapping.
- `input` maps approved PNG coordinates to EIS, resolves XKB input, and owns
  synchronization and held-state cleanup.
- `session` validates the active Wayland session and reports whether the
  process is actually isolated. It never starts a compositor. The only
  supported launcher is `scripts/run-isolated-session.sh`.
- `takeover` watches physical input and cooperative handoff signals. Its latch
  is checked by mutation and wait paths.
- `virtual_desktop` provides best-effort KWin desktop discovery and switching
  for activation evidence. It never turns an unknown desktop location into an
  authority claim.

The workspace and packaged native skill are checked by
`scripts/check-guidance.sh`; the skill is documentation, not a runtime route.

## Identity and Catalogs

The standard foreign-toplevel protocol and the optional KDE protocol do not
provide an authoritative cross-protocol join key. The catalog keeps their
records separate and reports unsupported, unavailable, or busy authorities
instead of fuzzy-merging them. AT-SPI records use exact accessibility object
identity and PID information; AT-SPI does not provide a compositor desktop ID.

Application and window IDs are opaque and valid for one process and backend
lifetime. A disappearance, replacement, PID change, or backend identity change
creates new IDs. Titles, names, PIDs, geometry, and traversal order are
descriptive data, not target keys. Pagination cursors are tied to catalog
membership generations.

`window_opened` polls the catalog for an exact compositor `app_id`. It accepts a
requested ID with or without `.desktop`, normalizes that suffix on both sides,
and never falls back to a title or AT-SPI application name. It has presence
semantics and returns the matched opaque target; it does not prove an arrival
event. `window_closed` first requires an authoritative observation of the exact
opaque window ID, then waits for that ID to be absent. An unavailable catalog
cannot be treated as closure.

Protected-surface detection is a heuristic over descriptive metadata. It
recognizes known authentication, privilege, permission, locker, and pinentry
patterns and exposes `is_protected_surface`; it is not a complete security
classifier.

## Execution

The MCP process starts idle. The first discovery or launch call creates only its
selected worker, defaulting to foreground when no desktop or returned identity
selects a route. `desktop` is an explicit selector only for discovery, launch,
and targetless `window_opened`; targets, observations, and cursors route by
their returned opaque IDs. There is no session ID input.

Foreground portal approval is requested on demand. The worker retries known
local approval failures until its 45-second deadline, using 1, 2, 4, and 8
second backoff, and stops on call cancellation, shutdown, or unspecified worker
termination. The background worker is started lazily inside the private runner.
It owns its private services and bus, installs the KDE RemoteDesktop
PermissionStore authorization on that private bus/data, and does not alter the
physical session's permissions. Each desktop has its own scheduling barrier.

An exhausted or revoked worker is retired. Its IDs become stale; the next
explicit discovery or launch creates a new worker without requiring an MCP
restart. Cleanup completes before the next call on that desktop starts.

An `act` call re-resolves target and accessibility identity before dispatch.
Spatial input also revalidates the exact source frame, portal session, stream,
format, route, dimensions, and EIS device. A newer frame may confirm unchanged
metadata but cannot replace the coordinates from the approved source frame.
Each attempted mutation reports dispatch, cleanup, and post-action evidence
separately. None of those stages proves application delivery, seat focus, or
application effect.

KDE minimize, maximize, restore, and close calls report request acceptance,
protocol send, and connection flush separately. The Wayland backend defers a
successful state or close reply until the request is flushed. A queued request
that is cancelled before dispatch is retryable and does not mutate later. A
request cancelled or interrupted after dispatch has an unknown outcome. Restore
reports each state subrequest separately.

## Session and Takeover

The process binds the display named by `WAYLAND_DISPLAY`. Startup requires
`XDG_SESSION_TYPE=wayland`, a real Wayland socket, and a live Wayland display.
`COMPUTER_USE_MCP_DISPLAY` is diagnostic validation only; it does not change
which display client libraries use. If it disagrees with `WAYLAND_DISPLAY`,
validation fails.

The isolated launcher creates private runtime, configuration, data, and state
directories and starts real private instances of D-Bus, KWin, PipeWire,
WirePlumber, the AT-SPI bus and registry, and both portals. It requires
`kwin_wayland`; there is no cage or gamescope compositor fallback. It writes a
readiness marker only after process ownership, environment, socket, and service
checks pass. The server verifies the marker and the actual endpoints before
calling a session isolated.

This isolation separates routing and cooperative same-user processes. It is not
a malicious same-user sandbox. A same-user process with sufficient host access
can still inspect or interfere with the session.

Takeover watching is operation-scoped: it is armed while an active mutation or
wait is running, not for an idle MCP process. Readable physical
`/dev/input/event*` devices, the cooperative environment or handoff file, and
EIS physical-modifier refusal are independent signals. Watching is skipped only
after isolation is verified. On detection, the latch remains set for the MCP
lifetime. Clearing the signal does not resume work; resumption requires user
authorization, clearing the signal, restarting MCP, and taking a fresh
observation. Cleanup joins the operation-owned watcher and releases held input,
but cannot retract events already delivered.

## Capture and Mapping

PipeWire negotiation requests CPU-readable shared memory, `MemFd` then
`MemPtr`. A DMA-BUF is usable only when PipeWire has already made it CPU
readable; native GPU DMA-BUF import is unavailable. GPU-only streams fail with a
diagnostic rather than using an unsafe or unimplemented path.

Frames retain format, stream, transform, crop, portal-session, and accessibility
generation metadata. `changed_rect` is an advisory tile-hash bounding box
against the previous committed captured frame. It is not a model observation
delta, and the first frame has no baseline, so its changed rectangle is unknown.
Capture keeps a complete first frame as the baseline when a new stream has not
yet produced a newer frame; it does not reject that valid first frame merely
because the producer has not settled.

Target-window crop converts KDE logical geometry to raw source pixels before
transform, downscale, and PNG encoding. The mapping retains that source crop so
input and frame waits use the same coordinate space. A crop requires an
authoritative portal monitor position and logical extent. Missing, invalid,
out-of-frame, or untrusted geometry falls back to the full monitor with an
explicit reason. Multi-region EIS union mapping also requires the authoritative
portal stream extent; aspect ratio alone is insufficient.

## Accessibility and Input

AT-SPI traversal and text reads are bounded. Selected text is range-capped
before `GetText`, and value reads use bounded `CurrentValue`. Text replacement
requires `Component`, `Text`, and `EditableText`, focuses the target, performs
the replacement, and verifies a fresh full-text readback. Replacement values are
not logged. Replacement insertion uses UTF-8 byte length and verification
compares the exact returned text and Unicode scalar count. The project
intentionally does not promise password redaction.

`paste` prefers the portal Clipboard capability when the portal reports
`clipboard_enabled`. The request is made before `RemoteDesktop.Start`; the
runtime accepts only bounded `text/plain` transfer, answers `SelectionTransfer`
through `SelectionWrite` and completion, sends `Ctrl+V`, waits for the transfer,
and clears the selection. It does not read or restore the user's previous
selection. If the capability is unavailable, paste performs one point-focus
click and streams bounded 4,096-scalar chunks through simulated EIS typing in
the same cleanup-safe transaction.

Focus grace is a one-shot, best-effort shortcut after a successfully dispatched
click whose mapped point is inside the current KDE geometry. It matches the same
accessibility app and window identity, is consumed once, and is invalidated by a
focus or geometry change, takeover, or expiry. A false `Component.GrabFocus`
result is accepted only after a fresh exact element-focused and window-active
readback. Unknown focus state is reported as unavailable or null, never inferred
as focused.
