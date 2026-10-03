# Security

[MCP.md](MCP.md) describes caller-visible behavior. This document defines the
trust boundary, safeguards, and residual uncertainty.

## Trust Boundary

Run the server only for a trusted local MCP host and user. AT-SPI reads and
semantic actions do not require a portal prompt. Portal approval grants a
session, not approval for each later call. A screenshot contains the complete
selected monitor, including unrelated windows and notifications.

Accessibility views bound traversal and text output, but screenshots are not
password-redacted. Replacement values are not logged, and diagnostics do not
log typed or assigned values or portal restore tokens. The project intentionally
does not promise generic password redaction. Replacement length accounting uses
UTF-8 bytes and verification compares the exact returned text.

The bounded call history stores operation metadata, opaque references, timing,
error codes, and dispatch/cleanup status in private XDG state files. It excludes
raw arguments, supplied text and values, clipboard data, observation text,
screenshots, and error messages. Recording failures are diagnostic only and
cannot alter the outcome or replay a call. See [MCP.md](MCP.md#call-history)
for retention and inspection commands.

Protected-surface detection is a heuristic over application and window
metadata. Known authentication, privilege, permission, locker, and pinentry
surfaces are refused before `act` or `activate_window` dispatch. The heuristic
is useful defense in depth, not a complete classifier.

## Safeguards

- Targets, observations, frames, elements, cursors, and mappings are opaque and
  generation-bound. Missing, replaced, stale, changed, or ambiguous identities
  fail closed.
- Launch accepts only an exact installed GIO `.desktop` ID. It accepts no
  command, path, arguments, subprocess, X11, direct-device, or clipboard
  fallback.
- Semantic actions revalidate application, window, PID, accessibility object,
  role, name, interfaces, and defunct state. Capability read failures do not
  create authority.
- Spatial input requires the exact source mapping, current portal session and
  stream, format generation, route, device, target identity, and cache
  generation. Coordinates use half-open PNG bounds and are never clamped.
  AT-SPI extents are not coordinate authority.
- Target-window crops use KDE logical geometry and retain an exact mapping back
  to the approved EIS region. Missing or untrusted geometry falls back to the
  full monitor with an explicit reason.
- Generated input uses the portal EIS connection. A central cleanup guard
  releases held keys and buttons on success, error, timeout, cancellation, EOF,
  session close, and shutdown. Cleanup completes before another call starts.
- Physical shortcut modifiers cause keyboard input to fail closed. Point focus
  and keys share one cleanup-safe transaction. Protocol synchronization proves
  progress only; it does not prove seat focus, application delivery, or effect.
- Semantic focus requires an AT-SPI focus grab followed by fresh exact
  element-focused and window-active reads. A false or failed `GrabFocus` is
  accepted only when those fresh reads verify the same element and active
  window. One-shot click grace is separate best-effort evidence and never proves
  seat focus.
- `paste` uses the portal Clipboard capability when `clipboard_enabled` is
  returned. It accepts bounded `text/plain` transfer, sends `Ctrl+V`, waits for
  completion, and clears the selection. It never reads or restores the user's
  previous clipboard, and the text is not logged. If the capability is
  unavailable, it uses bounded simulated EIS typing.
- Restore tokens use private XDG state storage and are never logged.

## Broker Workers

The broker has independent lazy foreground and background workers. Calls may
select `desktop: "foreground"` or `"background"` only for discovery, launch,
and targetless window-open waits. Without a selector, returned opaque IDs and
cursors route later calls automatically; callers do not provide a `session_id`.
Public IDs are namespaced so identities from separate workers cannot collide.

Foreground portal approval is requested on demand, with a known local 45-second
deadline and 1/2/4/8-second backoff. Cancellation and shutdown stop the retry.
Approval exhaustion or revocation retires the worker and invalidates its IDs;
the next explicit discovery or launch creates a replacement without restarting
MCP. Background workers own an embedded private runner and force KDE
PermissionStore authorization on their private bus/data only. They do not
change physical-session permissions.

## Session Isolation

Each worker binds its actual Wayland display. Portal, capture, EIS, and the
window catalog inherit that worker's process environment. Startup requires a
Wayland session, a real display socket, and a live display. The display override
is diagnostic validation only; it cannot redirect client libraries, and a
mismatch with `WAYLAND_DISPLAY` fails validation.

`scripts/run-isolated-session.sh` provisions private runtime, configuration,
data, and state directories and starts real private D-Bus, KWin, PipeWire,
WirePlumber, AT-SPI, and portal services. It requires `kwin_wayland` and has no
fallback compositor. The server verifies process ownership, environment,
private endpoints, sockets, and readiness before accepting the isolated verdict.

This is routing and process separation for cooperative processes running as the
same user. It is not a malicious same-user sandbox. A same-user process with
host access may still inspect or interfere with the session. Isolation is also
not a substitute for portal consent or an MCP host permission review.

## Takeover

Physical monitoring runs throughout the foreground worker's lifetime.
It watches readable physical `/dev/input/event*` devices, cooperative
handoff signals, and EIS refusal caused by physical Ctrl, Alt, Super, or latched
modifiers. Agent EIS events do not appear as physical device events. A verified
isolated session skips the physical watcher; an unverified session does not.

Detection is best effort. If no input device is readable and no cooperative
signal is present, an unannounced physical handoff may be invisible. The
feature does not invent an input path or block ordinary work when no signal is
available.

Human activity refuses or interrupts foreground mutations with `HumanInputBusy`.
The runtime attempts generated-input cleanup and desktop restoration, reporting
their result separately from dispatch. Interrupted operations stay aborted;
cleanup cannot retract delivered events. Read-only calls remain available.

Mutations may resume after 60 seconds of quiet with observed physical keys
released. Cooperative handoff signals block until cleared, then start a quiet
period. The explicit `human_idle` wait reports unavailable physical monitoring
rather than claiming idle. Resumption requires a fresh observation, without an
MCP restart. Cleanup failure remains a separate session failure. Human activity
never redirects work to background; choosing it requires the user's request.

## Residual Uncertainty

Desktop state can change between every observation and action. A request may
have reached the desktop before cancellation, a lost reply, cleanup failure, or
post-action observation failure. `unknown` means dispatch may have started;
`completed` may still have incomplete later evidence. Observe current state
before deciding whether to act again.

Native GPU DMA-BUF import is unavailable. PipeWire negotiates `MemFd` or
`MemPtr`, and GPU-only buffers fail closed. Adding native import requires an
explicit safe design compatible with `forbid(unsafe_code)`.

The private-session `doctor` smoke passed, establishing startup support. The
direct normal-MCP smoke also passed screenshot, click, focused typing, portal
Clipboard paste, and fresh AT-SPI readback. It verified clean process teardown:
the target was gone and no owned descendants remained. Its window-close case was
unavailable because the AT-SPI target lacked KDE authority; that is a capability
boundary, not a teardown failure.

The `changed_rect` frame field compares the current capture with the previous
committed capture. It is not a model observation delta and the first baseline is
unknown. A complete baseline may still be returned when a new stream has not
produced a newer frame during startup capture. Multi-region coordinate mapping requires the authoritative portal
extent; a matching aspect ratio is insufficient.

Report security issues privately. Do not include screenshots, private text,
selected text, keys, field values, or restore tokens unless the report channel
is appropriate for that data.
