# Security

See [MCP.md](MCP.md) for caller-facing behavior. This document records the
stable threat boundary and fail-closed safeguards.

## Trust boundary

Run the server only for a trusted local MCP host and user. AT-SPI has no portal
consent boundary: it can expose private text and invoke controls with side
effects. Installed-app launch is also independent of portal approval. The portal
approves a session, not each call, and a screenshot contains the complete
selected monitor, including unrelated apps and notifications.

Visible and interactive accessibility views reduce tree disclosure but do not
redact screenshots. A screenshot does not prove that a target is visible or
unoccluded. Routine images remain in memory; protocol stdout contains only MCP
frames, and diagnostics omit typed/assigned values and restore tokens.

## Stable safeguards

- Targets, observations, frames, elements, catalog cursors, and mappings are
  opaque and generation-bound. Replaced, evicted, stale, missing, changed, or
  ambiguous identities fail closed.
- Launch accepts only an exact case-sensitive ID from the installed GIO catalog;
  no command, path, arguments, clipboard, X11, direct-device, or subprocess
  fallback exists. Launch clears observations and returns only acknowledgement.
- Semantic actions revalidate PID, app/window and element object identity, role,
  name, interfaces, and non-defunct state. Timeouts and failed capability reads
  cannot create authority.
- The user chooses the single shared monitor. AT-SPI geometry is never used to
  crop it or derive input. Coordinates use exact half-open PNG bounds, are never
  clamped, and normalize only into the approved EIS region.
- Spatial input requires the exact source mapping plus current portal session,
  stream, format generation, route/device grant, target identity, and cache
  generation. Newer frame metadata may confirm but never replace source
  coordinates. Any mutation clears all retained mappings.
- Generated input uses only the portal EIS connection. Keys and buttons have a
  central reverse-order cleanup guard on success, error, timeout, cancellation,
  session closure, EOF, and shutdown; cleanup is awaited before another call.
- Keyboard input fails closed with active physical shortcut modifiers. Its point
  click and keys share one cleanup-safe transaction. Synchronization proves only
  protocol progress, not seat focus, application delivery, text delivery, or
  effect. Semantic focus grants no keyboard authority.
- Restore tokens are stored with private XDG state permissions and never logged.
  Persistence can be disabled for a run with
  `COMPUTER_USE_MCP_PERSIST_PORTAL=0`; this neither erases an existing token nor
  revokes a portal-side grant.

## Residual uncertainty

Desktop state can race every observation. A semantic or generated action may
have reached the desktop before cancellation, a lost reply, cleanup failure, or
post-action observation error. Cleanup can release held state but cannot retract
accepted events. Treat `unknown` and `completed` outcomes as uncertain final
state and observe before deciding whether to act again.

Report security issues privately. Do not include screenshots, private text,
tokens, selected text, keys, or field values unless requested through a secure
channel.
