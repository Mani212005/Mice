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

## M5 selection actions

- M5 is implemented without any drag interception. Normal text/table/image
  selection stays entirely with the foreground application.
- After text is selected, **Control double-tap** invokes a summary and
  **Control+Option+I** invokes an infographic. Both defaults are configurable
  from `mice settings`; the available alternates are Control+Option+S and
  Control+Option+M respectively.
- The agent reads `kAXSelectedTextAttribute` from the focused element first.
  When an app does not expose selected text, it snapshots the pasteboard,
  briefly synthesizes Cmd-C, reads text/HTML, and restores the snapshot before
  MICE sends anything to a model. Nothing is persisted.
- The fallback preserves the user's earlier clipboard; after a successful MICE
  action, the generated summary or PNG deliberately becomes the new clipboard
  content, matching existing M2 behavior.

## Task-planning feature — intended direction

- MICE should accept a task request, use the selected cloud/local provider
  according to user preference, and return an understandable sequence of
  steps.
- Any future execution must be confirmation-gated per step. It must not
  autonomously submit forms, handle credentials, make purchases, or complete
  sensitive actions such as bank-account registration. In those contexts MICE
  provides guidance and waits for the user to act.
- The native prompt/UI for task planning and its preference controls are not
  implemented beyond M6a's plan/review stage.

## M6a Goal Guide — plan and review

- **Control+Option+Space** opens the native goal prompt. This default is
  configurable in `mice settings` and intentionally does not take Spotlight
  or Finder search shortcuts.
- The portable core owns a small `GoalSession` state machine:
  `AwaitingGoal → Planning → Reviewing → Accepted`. The user can submit a
  revision from review or accept an unchanged plan.
- OpenAI and Groq plans use strict/JSON-object schemas with 3–8 advisory
  steps. Each step has an app hint and a `sensitive` marker. Prompts explicitly
  prohibit claims that MICE clicks, types, submits forms, logs in, or handles
  credentials.
- M6a ends after plan acceptance. It deliberately does not yet create a guide
  panel, observe completion, or act on the plan. Those capabilities belong to
  M6b–M6d and remain confirmation-gated.

## M6b Goal Guide — manual progress and native highlights

- Accepting a reviewed plan now enters a manual guide loop. Every step presents
  **Next**, **Back**, and **Quit**; the user decides when an instruction is
  complete. Reaching the final Next only marks the guide complete—it performs
  no external action.
- The macOS agent makes a bounded, read-only search of the focused native app's
  AX tree using the current step's app hint and instruction. A matching visible
  element receives the existing cyan highlight overlay. Failure to find a
  target is non-fatal and never substitutes a guessed click.
- Browser-specific highlighting is still M6c. M6b deliberately uses only the
  native AX surface and does not extend browser-extension permissions or let a
  model drive UI actions.

## M6c Goal Guide — browser highlights driven by core

- The original token bridge is superseded by M11a native messaging. It carries
  only the current browser-hinted Goal Guide instruction and does not persist
  plans, DOM data, or browser content.
- The browser extension wakes its background worker from the active tab,
  requests the active directive, sends a fresh bounded DOM snapshot, and uses
  the existing candidate-ID validation before highlighting the returned
  selector. The core rejects stale session/instruction pairs.
- Browser highlighting remains a visual aid. The extension scrolls and outlines
  a verified target but never clicks it, enters text, submits a form, or handles
  credentials. M6d completion checks remain optional and user-triggered.

## M11a — invisible browser companion

- Browser Goal Guide transport now uses Chrome native messaging relayed over a
  user-owned Unix socket at `~/Library/Application Support/MICE/bridge.sock`.
  The socket is created with mode 0600, so a runtime token and localhost poll
  loop are no longer required for MICE's normal `start` flow.
- `mice setup-browser` installs the native-host manifest, while `mice
  native-host` is launched by Chrome and only relays framed JSON between
  Chrome's standard streams and the socket. The extension ID is deterministic
  from its checked-in public key; no private key is in the repository.
- The extension is intentionally invisible: no popup, options page, token
  entry, or persistent user configuration. It accepts pushed current-step
  directives and returns a fresh bounded snapshot for the existing verified
  candidate-ID highlight path. Acting is still out of scope until M11b.

## M11b — confirmed browser actions

- Eligible browser-hinted steps now expose **Do it**. MICE first presents a
  one-action preview; Cancel returns to the same guide step, while Confirm
  sends one command only to the selector already verified from the current DOM
  snapshot.
- Click is the default. Steps whose wording asks the user to type, enter,
  write, or fill show a transient input field and send only that user-supplied
  value. Values are neither inferred nor persisted.
- The content script rechecks visibility before acting and returns a result via
  native messaging. Confirmed actions produce a terminal audit line. Sensitive
  plan steps remain highlight-only; M11c will add the stronger independent
  password/payment/final-submit blocklist.

## M11c — actions blocked for sensitive controls

- Two independent checks now protect browser actions. Rust uses the verified
  candidate label/role and step wording to refuse credential, OTP, payment,
  authentication, transfer, and final-submission actions before dispatch.
- The content script checks the live target again. It refuses password inputs,
  `autocomplete="one-time-code"`, `autocomplete="cc-*"`, payment-like field
  labels, and sensitive click labels; it also refuses submit controls inside a
  form containing password, one-time-code, or payment fields.
- Refusal leaves the control highlighted and tells the user that the action is
  theirs. Sensitive plan steps were already highlight-only and remain so.

## M12 — bounded Web Autopilot

- Autopilot is a cloud-only, consented browser loop rather than a static plan:
  fresh bounded observation → one strict candidate-ID decision → one verified
  action → observe again. The portable core limits runs to 15 actions and
  hands off after two failures on the same target; the CLI also caps a run at
  15 minutes.
- The extension observes SPA navigation and meaningful interactive-DOM changes.
  Sparse pages request one bounded active-tab JPEG; that vision turn routes to
  OpenAI even when Groq is the configured DOM provider. No screenshot is
  persisted.
- Hard stops remain doubled in Rust and the content script. Autopilot will not
  enter credentials/OTP/payment data or click authentication, payment,
  transfer, purchase, or final-submit controls. It narrates the handoff.
- `mice autopilot "<goal>"` asks for goal-level consent. `[autopilot]`
  defaults to `persona = "patient"`; the first successfully completed goal
  asks before each safe action, then records `first_run = false`. Users can
  opt into per-action confirmation permanently with `careful_mode = true` in
  `mice settings`. Esc sends a native stop notification while an autopilot
  session is active.
- Chrome launches native-messaging executables directly rather than with a
  product subcommand. MICE detects Chrome's extension-origin/framed-stdin
  launch and enters its relay automatically, avoiding a host-exit retry loop.

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
