---
name: computer-use-mcp
description: Operate a Wayland desktop using screenshots, accessibility, and window targets.
license: MIT
---

# Desktop workflow

1. `list_desktop` discovers windows and installed `desktop_id`s. Launch, then
   find the window on that desktop. Keep compositor and AT-SPI IDs separate.
2. Use `activate_window` for window controls; require advertised capabilities.
3. Observe accessibility for semantics, screenshots for coordinates. Narrow
   scope before raising limits. Frame waits return metadata, not pixels.
4. Prefer `invoke`/`set_value`. Modal-opening menus can block invoke: use a
   screenshot click or advertised keybinding, never guessed coordinates/shortcuts.
5. Exact editor saves may append a newline; inspect saving options first.
6. Keep navigation separate; text+submit may share one keyboard transaction.
   Adopt readback IDs/target; it lists only changes, unlisted IDs hold.

`window_opened` requires compositor app-ID metadata; otherwise `list_desktop`.

Human input stales foreground evidence. To act, wait `human_idle` and observe again.
