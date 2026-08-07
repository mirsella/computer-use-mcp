# Security

See [MCP.md](MCP.md) for the user-facing tool, state, coordinate, and error
contract. This document describes the trust boundary and safeguards.

AT-SPI access is powerful and does not use an XDG portal consent prompt. A
process in the same graphical session can read text exposed by accessible apps
and invoke their semantic actions. This can include private messages, document
contents, form values, and controls with real side effects. Run this server only
for a trusted local MCP host and user. Installed-app launch is also independent
of portal approval.

Current safeguards:

- `act_on_element`, `pointer`, and `keyboard` require an exact retained ID from
  the same process. The cache is bounded and retains only the latest state for
  each app/window target; replaced, consumed, and evicted states are rejected;
- application launch accepts only a full, exact, case-sensitive `desktop_id` from
  installed `list_applications` and launches its exact GIO app record; callers cannot supply a
  command, path, or arguments;
- app lookup never chooses an ambiguous match;
- launch clears the observation cache and returns an acknowledgement; callers must observe before acting;
- cached PID, exact app/window identity, and generation-scoped elements are checked;
- semantic actions require the same non-defunct accessible object, role, and name;
- unsupported interfaces, timeouts, and identity failures stop the action;
- diagnostics go to stderr and do not include typed text or assigned values;
- portal restore-token values are never logged and are stored only with private
  XDG state permissions. Persistent restoration is requested by default and can
  be disabled for a run with `COMPUTER_USE_MCP_PERSIST_PORTAL=0`; this does not
  erase an existing token or revoke the portal-side grant;
- the portal chooser, not the client, selects the one shared monitor;
- screenshots contain the complete monitor selected by the user; no AT-SPI
  global window origin is trusted or used to crop it;
- the PID and app/window identities are re-read after frame acquisition and
  before cache binding;
- routine screenshots remain in memory and are not written to disk;
- MCP stdout contains protocol frames only;
- generated input requires the exact cached app PID and app/window identities,
  observation generation, live portal session and stream, and actual
  RemoteDesktop device grant. Pointer and point-focused keyboard actions also
  require an in-bounds full-monitor PNG point;
- coordinates are never clamped and no global scale is inferred;
- pointer movement uses the same single-use screenshot mapping as clicks but
  emits no button event; hover effects and auto-hidden panels may still change;
- generated input uses only the approved portal session's EIS connection; no
  portal `Notify*`, X11, direct-device, clipboard, or subprocess fallback exists;
- held keys and buttons use a central reverse-order cleanup guard on success,
  error, timeout, cancellation, session closure, EOF, and shutdown. Cleanup
  failures are logged and do not replace the original action error. A press is
  tracked before its async send, and an awaited cleanup barrier runs before the
  next mutation;
- current PipeWire health, frame dimensions, crop, and transform must still
  match the screenshot mapping before generated input starts. Its format
  generation must also match, and a newer frame is allowed only when that
  metadata remains unchanged.
- any semantic or generated mutation clears every retained screenshot mapping,
  including mappings for other cached targets, because the monitor pixels may
  have changed;
- generated keyboard input fails closed when physical Ctrl, Alt, Super, or a
  latched modifier is active immediately before keyboard emulation, and modifier
  state is synchronized after each keyboard action. Physical modifier changes can
  race a multi-key transaction because EIS cannot distinguish them from synthetic
  held modifiers; do not type during generated input. Point focus clicks an
  in-bounds PNG point. Element focus revalidates the exact AT-SPI object, calls
  `Component.GrabFocus`, emits no pointer event, and requires a post-focus read
  reporting that element focused and its window active. AT-SPI state can still
  race compositor seat focus. App names and focus-switch shortcuts such as
  Alt+Tab are not keyboard-routing primitives.

There is no portal permission boundary around AT-SPI reads or semantic actions.
The monitor screenshot may expose unrelated apps, notifications, desktop files,
or other private content. Screenshots do not gate semantic actions or prove that
a target is unoccluded.
Visible and interactive observation views reduce accessibility-tree disclosure,
but they do not crop or redact the complete-monitor screenshot.
A semantic action may have completed before a post-action refresh reports an
error; callers must treat that error as uncertain final state, not proof that
the action did not occur.

Capture and input bind each PNG to its approved stream, frame, target generation,
and EIS region. Invalid, stale, missing, changed, or ambiguous mappings fail
closed. Element frames remain `atspi_window_coordinates`; point input uses
`screenshot_png_pixels` normalized into compositor-private EIS coordinates.

The server cannot prove that an AT-SPI semantic action had no effect when its
reply is lost or cancellation happens during the call. Generated input cleanup
also cannot retract an event the compositor already accepted; it only releases
state that may remain held.

Report security issues privately. Do not include screenshots, private text,
tokens, selected text, keys, or field values unless a maintainer requests them
through a secure channel.
