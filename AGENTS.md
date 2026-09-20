# Working on computer-use-mcp

This Rust server exposes six desktop tools. Keep tool meanings in
`src/contract.rs` and parsing in `src/validation.rs` consistent, under
`crates/computer-use-mcp/`. `MCP.md` is the protocol reference; `ARCHITECTURE.md`
explains ownership and dispatch. Read the relevant sections, not every document.

## Boundaries that matter

- Opaque IDs identify exact backend objects and observation generations. Never
  join windows by title/PID or infer image coordinates from AT-SPI bounds.
- Dispatch progress determines retry safety. Cancellation after any mutation is
  not `not_started`; protocol completion is not application effect.
- One generated-input operation owns focus, input emission, and cleanup. Its
  source crop, transform, and mapping must describe the actual returned image.
- The private runner owns its bus, compositor, and services. Environment names
  alone do not establish isolation. Live testing uses that runner, not the
  physical desktop unless the user explicitly requests physical interaction.
- Keep `unsafe_code = "forbid"`. Observation text is intentionally unredacted;
  diagnostic logs must not echo supplied text or clipboard contents.

## Changes and checks

Add regression tests for real state transitions and protocol arguments, not a
fake that duplicates the implementation. Run relevant tests after the complete
change, then workspace checks for cross-cutting changes. See `DEVELOPMENT.md`
for commands and host toolchain notes. Keep Cargo artifacts on disk, not `/tmp`.

Agent guidance has three jobs: initialize gives universal evidence/recovery
rules, tool descriptions explain each operation, the skill teaches the workflow.
Keep both skill copies byte-identical with `scripts/check-guidance.sh`. Measure
serialized tool/result budgets when changing the contract. Do not add repeated
instructions or claim unverified live capabilities. `plan.md`, when present,
is local planning material; maintained docs and tests define current behavior.
