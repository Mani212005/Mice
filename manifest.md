# MICE Manifest

Compact implementation record. Architectural and product decisions are in
[`decisions.md`](decisions.md); no credentials, captures, clipboard contents,
models, or user configuration belong in this repository.

## Product and boundaries

- Terminal-first macOS product: portable Rust core plus thin Swift agent.
- Rust owns routing, state, providers, prompts, and clipboard representations.
- Swift owns macOS permissions, input, capture, Accessibility, pasteboard, and
  native overlays.
- Agent/core communication is only `mice-ipc` length-prefixed JSON-RPC.
- Normal input is pass-through; a confirmed configured gesture may be consumed.
- Default local model: `gemma3:4b`; smaller alternative: `phi4-mini`; heavy
  `gpt-oss:20b` remains opt-in after hardware preflight.

## Milestones

| Milestone | Status |
| --- | --- |
| M0 native probes | Complete; macOS permissions manually verified. |
| M1 IPC spine and cloud flow | Complete. |
| M2 actions, clipboard, local lane, settings | Implemented; manual image/paste acceptance deferred. |
| M3 hover and browser guide-me | Functionally accepted; visual highlight polish deferred. |
| M4 packaging and Linux preparation | Started; Linux handshake scaffold exists. Apple refinement is the active priority. |
| M5 selection actions (summarize / infographic gestures) | Planned; see `plan/mice_planv2_selection_and_goals.md`. |
| M6 Goal Guide (goal popup → plan → step-by-step guidance) | Planned; staged M6a–M6d in the same plan. |

## Current capabilities

- MICE supports local, OpenAI, and Groq provider paths. Runtime environment
  variables hold keys; `cloud_allowed`, `cloud_only`, and `local_only` are
  configurable. `cloud_only` routes text/hover work to the configured cloud
  model rather than Ollama.
- Hover explanation requires **Control + hover** for roughly 650 ms. It uses
  current AX data, hides raw AX roles/tooltips, prefers actionable descendants,
  strips streamed ANSI control sequences, and bounds model context.
- Browser guide-me uses the `browser-ext` Chrome extension and a localhost,
  token-protected bridge. It ranks and bounds DOM candidates, uses verified
  candidate IDs rather than model-generated selectors, and supports OpenAI or
  configured Groq JSON output.
- Native app selection remains pass-through. Use the app’s normal selection and
  Cmd-C/Cmd-V to preserve source text/table/image clipboard formats. A future
  non-persistent clipboard observer may enrich these native representations.
- `agent-linux` implements the shared handshake only and advertises no Linux
  desktop capabilities yet.

## Recent repairs

- Fixed Ollama prompt `E2BIG` by sending prompts through standard input.
- Added a true `cloud_only` mode and routed it to the configured cloud model.
- Made hover explicit (Control + hover) and reset its fingerprint on release.
- Fixed browser guide candidate sizing, ranking, provider selection, and
  candidate-ID validation.
- Fixed macOS agent IPC reads to accumulate partial pipe reads, preventing large
  clipboard/image frames from terminating the agent.
- Re-enable the macOS event tap after timeout/user-input disable events.
- Forward action-preset instructions to all model streaming paths.
- Block `Action::Guide` in local-only routing.
- Prevent `mice ask` from waiting for EOF when stdin is an interactive TTY.

## Verification

- `swift build` in `agent-macos`
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`

## Active backlog

0. M5 selection actions, then M6 Goal Guide, per
   `plan/mice_planv2_selection_and_goals.md`. Selection is read only after an
   explicit keyboard gesture (AX selected-text first, pasteboard-restoring
   Cmd-C fallback); mouse dragging stays fully pass-through. Triggers are
   configurable; defaults avoid Spotlight/Finder system shortcuts.
1. Carry captured PNGs into multimodal provider requests; current capture is
   OCR-first and does not yet provide real vision analysis.
2. Remove API keys from curl argument lists, preferably by moving provider HTTP
   calls to a Rust client.
3. Add `mice stop`, input-monitoring status, correct multi-display capture, and
   a lightweight/overlay-only mode for one-shot commands.
4. Address prompt/agent backpressure, stream error-body reporting, Unicode RTF,
   settings validation, and long-result overlay presentation.
5. Add a non-persistent native clipboard observer after user Cmd-C, then build
   the confirmation-gated task-planning interface.
6. Package/sign/notarize the macOS release when a Developer ID is available;
   defer PipeWire/portal/AT-SPI/libei implementation until Apple refinement is
   complete.

## Manual acceptance still useful

- M2: request an infographic from a selected table and paste into spreadsheet
  and rich-text destinations.
- M3: test a Control-hover explanation and a browser guide request on an
  unfamiliar control.
