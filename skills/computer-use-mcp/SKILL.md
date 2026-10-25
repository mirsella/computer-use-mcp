---
name: computer-use-mcp
description: Operate a Wayland desktop using screenshots, accessibility, and window targets.
license: MIT
---

# Desktop workflow

1. Discover windows with `list_desktop`. To open an app, discover its installed
   `desktop_id`, launch it, then find its window on the same desktop.
2. Use `activate_window` for window controls. Keep compositor and AT-SPI IDs
   separate; choose a target advertising the required capability.
3. Observe accessibility for semantic controls or screenshots for visual input.
   Narrow scope before raising limits.
4. Prefer advertised `invoke` or `set_value`, otherwise screenshot-grounded input.
   Separate focus/navigation, text entry and submission. Use replacement IDs;
   reobserve only if missing or insufficient.

`window_opened` needs compositor app-ID metadata. For AT-SPI-only windows use
`list_desktop`. Frame waits return metadata, not pixels; capture before choosing
new coordinates.

Human input stales foreground evidence. To act, wait `human_idle` and observe again.
