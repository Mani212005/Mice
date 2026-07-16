# MICE Decisions — Current Session

This file records the implementation decisions made during the current MICE
development session. It contains architecture and product decisions only; it
does not contain credentials, user configuration, clipboard contents, or
captures.

## Current product state

- M0 and M1 are complete.
- M2 is implemented; its manual GPT Image and destination-application paste
  checks remain deferred.
- M3 is functionally accepted. Browser highlight appearance is deliberately
  deferred for a dedicated visual/UI pass.
- M4 was started with a Linux IPC scaffold, but Apple refinement is now the
  active priority.

## Architecture boundaries retained

- Rust owns routing, state, provider choice, prompt construction, and
  clipboard representation generation.
- The Swift macOS agent owns AppKit overlays, input observation, screen
  capture, Accessibility access, and the native pasteboard.
- Rust and Swift communicate only through `mice-ipc` length-prefixed JSON-RPC.
- Normal global input is pass-through. MICE only consumes an explicit,
  confirmed gesture.

## Hover explanation

- Hover context is resolved from the current Accessibility element rather than
  stale terminal/model text.
- AX labels are made user-facing: actionable descendants are preferred over
  generic containers; raw AX roles and tooltip/help text are not presented as
  a control's identity.
- Ollama stream ANSI/OSC control sequences are removed before overlay display.
- Ollama prompts are passed through standard input, avoiding macOS `E2BIG`
  (`Argument list too long`) failures from large page context.
- Hover explanation is no longer passive. It requires **Control + hover** for
  roughly 650 ms; releasing Control cancels a pending request. Normal cursor
  movement never calls a model or opens an explanation overlay.

## Provider policy

- `cloud_allowed` permits cloud work but retains the original local-first
  routing for routine text/hover actions.
- `cloud_only` was added because the desired current workflow is cloud-only.
  It routes text and hover actions to the configured cloud model, currently
  Groq `llama-3.3-70b-versatile`, instead of attempting Ollama.
- `local_only` remains available for privacy-focused local routing.
- Browser guide-me uses OpenAI `gpt-5.6-sol` by default. When a Groq model is
  configured it can use Groq JSON Object Mode instead. No credentials are
  stored in this repository; API keys remain runtime environment variables.

## Browser guide-me

- The Chrome extension is in `browser-ext/`; it is a browser-specific DOM
  companion, not the cursor feature itself.
- Its purpose is to provide deterministic visible DOM controls and highlight a
  verified target. The native macOS cursor/hover feature remains separate.
- Controls receive unique selectors. The model does not generate CSS selectors:
  it chooses a supplied `candidate_id`, and Rust resolves that ID to the
  original selector.
- Candidate input is bounded to avoid provider request-size errors. The
  extension ranks controls against the request, emits at most 100 candidates,
  and Rust independently sanitizes, ranks, deduplicates, and supplies at most
  80 to the model.
- One guide request currently highlights one target. Browser highlight visual
  design is deferred for a later UI-focused pass.

## Smart copy and paste — current decision

- **The custom Shift/Option drag shortcut is disabled.** It is not currently a
  MICE feature.
- Reason: a screen rectangle is only pixels. It can preserve an image of a
  table, but cannot recover the owning application's editable table structure,
  rich text, links, or other native pasteboard representations.
- The attempted Shift-drag implementation consumed the mouse drag, which
  prevented Chrome and other apps from drawing their normal blue selection.
  That caused the observed inability to select, copy, or paste.
- MICE now leaves normal dragging completely pass-through. Select content using
  the app's native blue selection, then use **Command-C** and **Command-V**.
  This is the currently correct way to preserve native text/table/image formats.
- The next smart-clipboard implementation should be a non-persistent
  pasteboard observer that runs after a user-initiated Command-C. It may enrich
  already-copied native text/HTML/RTF/PNG representations, but must never
  replace them with a screenshot or interfere with selection.

## Task-planning feature — intended direction

- MICE should accept a task request, use the selected cloud/local provider
  according to user preference, and return an understandable sequence of
  steps.
- Any future execution must be confirmation-gated per step. It must not
  autonomously submit forms, handle credentials, make purchases, or complete
  sensitive actions such as bank-account registration. In those contexts MICE
  provides guidance and waits for the user to act.
- The native prompt/UI for task planning and its preference controls are not
  implemented yet.

## Linux preparation

- `agent-linux/` is a Rust scaffold that sends the shared IPC initialization
  handshake and advertises no unsupported capabilities.
- PipeWire, xdg-desktop-portal, AT-SPI, libei, Linux overlays, and Linux
  clipboard work are deferred until the Apple experience is refined.

## Verification most recently completed

- `swift build` in `agent-macos`
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`

