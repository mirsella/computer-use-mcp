---
name: computer-use-mcp
description: Operate the local Wayland desktop with exact MCP targets, fresh evidence, and explicit input boundaries.
license: MIT
---

# Computer-use MCP

Use only `list_desktop`, `launch_application`, `activate_window`, `observe`,
`act`, and `wait_for`. Read initialize instructions; there is no help call or
resource catalogue.

## Evidence-first workflow

Start with `list_desktop` using `{"scope":"windows"}` and copy one complete
opaque target. Use `applications` only for an installed `.desktop` ID. Titles,
names, PIDs, selectors, and guessed geometry are not IDs. Launch is only an
acknowledgement: list and observe the new target before acting. Use only exact
advertised capabilities and copy observation, frame, element, and cursor IDs
unchanged.

Activation evidence does not prove seat focus. `act` needs an exact
`source_observation`; pointer/keyboard also require its frame. Continue from a
returned replacement observation. Waits use exact prior IDs. On stale target or
cursor, list again; on stale observation or element, observe again.

## Input boundaries

Pointer and keyboard points are half-open pixels in the complete selected-monitor
PNG. Never convert AT-SPI bounds. Keyboard requires a visibly intended point and
either press-only events or one type-only event. Do not use Alt+Tab. Separate
routing, text, and submit with replacement observations; prefer advertised
`set_value`. AT-SPI focus is not keyboard authority.

## Uncertainty and privacy

Follow recovery for `not_started`. For `unknown` or `completed`, the action may
have happened: inspect current state and never repeat blindly. Protocol progress
does not prove focus, delivery, or effect. Restart only when recovery reports an
exhausted portal/session. Whole-monitor images and accessibility text are
sensitive; use only through a trusted local host.
