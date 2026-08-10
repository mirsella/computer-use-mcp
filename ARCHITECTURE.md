# Architecture

The public contract is [MCP.md](MCP.md). This document covers implementation
boundaries and internal authorities.

## Modules

- `contract` and `validation` own the ordered schemas and conversion of
  untrusted JSON into typed calls.
- `runtime` is the async desktop boundary; `server`, `cli`, and `errors` own
  transport and presentation.
- `accessibility` owns resolution, bounded traversal, observation caches,
  relocation, and semantic policy. `atspi_adapter` is the production AT-SPI/zbus
  implementation.
- `window_backend` reconciles opaque window catalogs and capability states.
  `wayland_catalog` runs Wayland I/O on a dedicated blocking thread and owns
  protocol lifecycle and KDE activation evidence.
- `desktop_launcher` resolves and launches installed GIO application records.
- `portal` owns RemoteDesktop/ScreenCast requests, grants, sessions, and restore
  tokens. `capture` owns the PipeWire thread and newest-frame channel;
  `geometry`, `encoder`, and `screenshot` validate and bind PNG mappings.
- `input` owns PNG-to-EIS normalization, device lifecycle, XKB resolution,
  synchronization, and held-state cleanup.

The native skill is documentation, not a runtime route. Its workspace and
packaged copies are checked byte-for-byte by `scripts/check-guidance.sh`.

## Catalog and observation identity

The standard foreign-toplevel backend supplies a conservative catalog. The
opt-in KDE backend replaces, rather than fuzzy-merges with, standard records:
the protocols have no authoritative cross-protocol join key. Bind and
single-client failures remain explicit while AT-SPI and capture continue.

Opaque app/window IDs bind backend identities for one process. Titles, app IDs,
PIDs, geometry, and traversal positions are descriptive only. IDs survive
metadata changes but not disappearance or backend lifetime changes. Pagination
is tied to catalog membership generation.

AT-SPI discovery opens the accessibility bus directly and reads bounded trees
through `AccessibilityAdapter`. Cached elements retain object identity, role,
name, depth, and validated extents. Filtering does not renumber generation-scoped
IDs. A bounded cache retains at most one observation per exact target and a
single-use screenshot mapping. A replacement affects that target; any mutation
clears every mapping because monitor pixels may have changed. Relocation requires
the same live object, role, and name. Replacement IDs are mapped by object
identity.

Screenshot observations refresh the catalog before capture and revalidate PID,
app identity, and window identity after frame acquisition. The committed mapping
contains session, stream, route, format generation, frame metadata, target and
accessibility generations, and PNG dimensions.

## Execution and cancellation

One background task initializes portal approval and capture while stdio starts
immediately. Calls that need the desktop session join that stable ready-or-failed
result. Execution then crosses one barrier: cancellation cleanup completes before
the next queued call starts. Stateful calls recheck generations after acquiring
the barrier. Cleanup has a deadline; failure closes the desktop session.

Before `act`, the runtime re-resolves target identity and accessibility state.
Spatial input also verifies the exact source mapping, current portal session,
stream health, format, route, dimensions, and EIS device. A newer committed frame
may confirm unchanged metadata but cannot replace source coordinates. After
dispatch, a bounded settle and refresh produces a replacement observation. If
dispatch completed but refresh fails, the result is `outcome=completed`, not
success.

One progress record follows each attempted mutation through dispatch, cleanup,
and independent visual/accessibility post-stages. These stages report request
and observation evidence, never application delivery, seat focus, or effect.
Response bounding preserves status, outcome, progress, and replacement IDs while
trimming only verbose projections.

## Portal, capture, and input

One RemoteDesktop session owns one user-selected monitor and its ScreenCast and
EIS grants. Portal request responses are subscribed before calls and filtered by
path; dropped requests are closed, `Session.Closed` is terminal, and replacement
restore tokens are stored privately. `ConnectToEIS` is one-shot. Setup failures,
timeouts, EOF, and disconnects close the session rather than switching routes.

PipeWire objects stay on one thread using the restricted portal FD. Streams bind
the v6 serial when available, otherwise the session-scoped node ID. Capture
accepts BGRx, RGBx, BGRA, or RGBA shared-memory buffers and validates crop,
transform, chunk offset, wrapped rows, stride, padding, and dimensions before
publishing an owned frame. DMA-BUF-only or corrupt streams fail closed. Each
format has a generation; renegotiation clears old frames. Each observation waits
for a frame produced after capture starts.

The complete transformed monitor crop is encoded without inferred desktop
geometry. PNG axes normalize directly into the selected private EIS region.
Routing uses `mapping_id`, or an exact unique resumed-region match when KDE omits
it. Changed or ambiguous routes fail closed.

A complete pointer or keyboard action is one EIS transaction with reverse-order
held-state cleanup. Point-focused keyboard input clicks, synchronizes, and
settles before keys; press phases synchronize separately. Physical shortcut
modifiers fail closed. Exact mapping metadata is rechecked around awaits, but a
whole-frame change epoch is not an authority because unrelated monitor animation
would invalidate otherwise sound input.
