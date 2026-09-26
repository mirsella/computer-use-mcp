---
name: computer-use-mcp
description: Operate a Wayland desktop using screenshots, accessibility, and window targets.
license: MIT
---

# Desktop workflow

1. Discover windows with `list_desktop`. To open an app, discover its installed
   `desktop_id`, launch it, then find its window on the same desktop.
2. Observe accessibility for semantic controls; request a screenshot for visual
   input. Narrow the accessibility scope before raising text or node limits.
3. Prefer advertised `invoke` or `set_value`. Otherwise use screenshot-grounded
   input. Separate focus/navigation, text entry, and submission into calls.
4. Continue from each action's replacement observation. Fetch another only when
   it is missing or insufficient to choose the next action or confirm the result.

`window_opened` needs compositor app-ID metadata. If the window is only exposed
through AT-SPI, discover it through `list_desktop` instead. A frame wait returns
metadata, not pixels; capture before choosing new coordinates.
