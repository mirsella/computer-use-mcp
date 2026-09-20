---
name: computer-use-mcp
description: Use the computer-use MCP to operate a Wayland desktop through screenshots, accessibility, and exact window targets.
license: MIT
---

# Computer-use MCP

## Route and discover

1. `list_desktop` with `scope: "windows"`; copy the complete target. To launch,
   list installed applications first. Launch only acknowledges a request; list
   windows again or wait for `window_opened`.
2. `observe` the target. Start with accessibility for semantic controls; request
   screenshots for visual grounding. Expand scope or text limits only when needed.
3. `act` from that observation. Prefer advertised `invoke` and `set_value`.
   Continue from the replacement observation and copy opaque IDs unchanged.

`desktop` is accepted only by `list_desktop`, `launch_application`, and a
targetless `window_opened`, with `foreground` or `background`. Omit it for the
foreground default. Returned targets and cursors route later calls; do not send
`session_id`. Background starts a private worker lazily. Retired-worker IDs are
stale; explicitly discover or launch again to create a replacement.

Use `activate_window` for switching, never Alt+Tab. Its other actions need KDE
window-management capability. Activation does not prove seat focus.

## Input

Pointer and point-focus keyboard/paste require `source_observation.frame_id`.
Use pixels in the returned PNG, `0 <= x < width` and `0 <= y < height`.
Check the crop and never convert AT-SPI bounds into image coordinates.

Keyboard/paste may use semantic focus without a frame. Semantic focus requires
fresh exact focused-element and active-window evidence; a false `GrabFocus` is
not enough. Click grace is one-shot best-effort evidence, not verified focus.
Use press-only keyboard transactions for shortcuts or one type event for text.
Reobserve between routing, text, and submit. A point-focused call clicks again.

`paste` uses bounded portal `text/plain` transfer and `Ctrl+V` when
`clipboard_enabled` is returned, then waits and clears the selection. It never
reads or restores the prior clipboard. Otherwise it uses bounded simulated EIS
typing. Check delivery evidence and the resulting field.

## Wait and recover

Window waits may omit target. `window_opened` checks presence by exact compositor
app ID, including an existing window; it never matches titles. `window_closed`
requires a previously observed exact window ID. Other waits require target. A
frame wait returns metadata, not a new image; observe before new coordinates.

Follow recovery instructions. On takeover, stop and ask the user; resume needs
authorization, cleared signal, MCP restart, and fresh evidence. Finish when fresh
evidence confirms the task result.
