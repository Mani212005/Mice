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
| M5 selection actions (summarize / infographic gestures) | Implemented; manual acceptance remains. |
| M6 Goal Guide (goal popup → plan → step-by-step guidance) | M6a–M6c implemented; browser transport is superseded by M11a native messaging. |
| M11 Guide-me that acts | Complete in the M12 companion flow: native transport, verified actions, and double-enforced sensitive-control blocklist. |
| M12 Web Autopilot & Companion | Implemented; **parked** pending an OpenAI key for vision. See `plan/mice_m12_review.md`. |
| M13/M15 execution manager | Implemented: deterministic CLI registry, local tool loop, MCP delegation surface, shared memory/artifact cache, workflow macros, capability advertisement, and savings ledger. |
| Product polish (interactive UI, selection intelligence, MCP, M7–M10) | In progress; Phases 1, 2a, 3 (local MCP server), and M7 file-scale summarization are complete. |

## Current capabilities

- MICE supports local, OpenAI, and Groq provider paths. Runtime environment
  variables hold keys; `cloud_allowed`, `cloud_only`, and `local_only` are
  configurable. `cloud_only` routes text/hover work to the configured cloud
  model rather than Ollama.
- Hover explanation requires **Control + hover** for roughly 650 ms. It uses
  current AX data, hides raw AX roles/tooltips, prefers actionable descendants,
  strips streamed ANSI control sequences, and bounds model context.
- Browser guide-me uses the `browser-ext` Chrome native-messaging companion.
  It ranks and bounds DOM candidates, uses verified candidate IDs rather than
  model-generated selectors, and supports OpenAI or configured Groq JSON
  output for DOM turns.
- **Web Autopilot (M12):** `mice autopilot "<goal>"` runs a consented,
  cloud-only observe → decide → act loop. It has a 15-action / 15-minute cap,
  compact history, exact candidate-ID validation, two-failure handoff, live
  page-change observation, terminal plus native-overlay narration, and Esc
  abort. Sparse browser snapshots request a bounded active-tab JPEG and route
  only that turn to OpenAI vision. Groq remains available for DOM-only turns.
  Passwords, OTPs, payment data, login/payment/transfer/final-submit actions
  are refused independently by Rust and the live content script.
- Native selection remains pass-through. After selecting text, **Control
  double-tap** summarizes it and **Control+Option+I** creates an infographic.
  MICE reads AX selected text first; AX-poor apps use a short synthesized Cmd-C
  fallback that restores the previous pasteboard before provider work begins.
- **M7 file-scale summaries:** local streaming uses Ollama's HTTP `/api/chat`
  endpoint with each local model's explicit context budget. Oversized local-only
  selections are structurally chunked and map-reduced with visible progress;
  oversized `cloud_allowed` selections visibly use the configured cloud model.
  Small selections remain single-shot. Ollama HTTP failures include the server
  error body, so a missing local model is reported clearly instead of as a bare
  404.
- **M7 follow-on fixes:** Go Deeper applies the same bounded routing and local
  map-reduce path as a large selection, so it does not overflow a local model
  after an otherwise successful file-scale summary. `mice stop` sends an
  owner-only shutdown frame to the running daemon's bridge socket.
- **Concise first summaries:** the normal selected-text summarize action asks
  the model for a natural, 500-character-or-less quick recap. It is a recap of the
  selection's purpose and two or three key points; Go Deeper is deliberately
  not capped. After the recap completes, MICE silently prepares one deeper
  answer in the configured provider/privacy lane. It is never shown, copied,
  or pasted unless the user presses **Go Deeper**; one background job at a time
  prevents local-model contention when selections change quickly.
- **Phase 3 local MCP server:** `mice mcp-server` provides stdio JSON-RPC MCP
  tools for `summarize_text`, `summarize_file`, `explain_code`, `define_word`,
  and `quick_answer`. These use only the configured local Ollama model; MICE
  never routes MCP tool text to a cloud provider. Large local summaries reuse
  M7's structural chunk-and-reduce flow.
- **Execution manager (M13/M15, v8 multipliers):** `mice tools` exposes a
  deterministic-first registry for Git, repository search, GitHub (`gh-axi`
  with `gh` fallback), Chrome AXI, and quota inspection. Results have a
  bounded return contract with an artifact reference; read-only results cache
  by tool arguments plus repository state. `mice do` runs bounded local tool
  loops on capable machines, while `mice mcp-server` exposes `run_tool`,
  `delegate_task`, `git_summary`, `repo_grep`, `memory_note`, `memory_query`,
  and `team_status` to every MCP-compatible harness. The shared file-backed
  memory store records bi-temporal events, derived facts/digests, artifacts,
  macro workflows, overlap warnings, and the `mice savings` ledger. Tool
  subprocesses receive a scrubbed environment without provider API keys.
- **M13 safety/cache repair (2026-07-18):** raw browser mutations are no
  longer exposed to MCP or the local tool loop; they fail closed until a
  fresh-snapshot, target-validation, per-action-confirmation executor is
  available. Browser snapshots, quota, and remote GitHub results are never
  persisted in the artifact cache; only repository-fingerprinted read-only
  results can cache, and those artifacts retain only bounded distilled text
  and token metadata—not raw captures or output. Artifact/macro names use
  SHA-256 keys with key verification; append-only memory writes now use an
  inter-process lock, single-buffer JSONL appends, and atomic derived-file
  publication. Workflow macros accept/replay read-only calls only, and local
  loop budgets are hard-limited to 1–12 actions.
- **Phase 2b Send to…:** completed text results offer a native Send to… menu.
  Its first destination pastes MICE's existing rich clipboard result into the
  app that is frontmost when Send to… is chosen (or the original app as a
  fallback). MICE first uses focused-field AX insertion, then falls back to a
  normal Command-V when Input Monitoring permits it. Escape dismisses the
  overlay only and remains pass-through to the foreground app.
- **Goal Guide (M6a):** press **Control+Option+Space**, describe a goal, then
  review, revise, or accept a 3–8 step advisory plan. Plans flag login,
  payment, account-setup, and personal-data steps as user-only. The flow has
  no automation, screen targeting, or step advancement yet.
- **Goal Guide (M6b):** accepting a plan opens a manual step dialog with
  **Next**, **Back**, and **Quit**. Before each step it performs a read-only
  AX label search in the focused native app and highlights a best-effort match.
  No match simply leaves the step unhighlighted; MICE never invokes the target.
- **Goal Guide (M6c):** browser-hinted steps publish only the current guide
  instruction through the native-messaging companion. The extension captures
  its active tab, the core validates a candidate-ID choice, and the extension
  highlights the verified selector—without clicking or typing.
- `agent-linux` implements the shared handshake only and advertises no Linux
  desktop capabilities yet.

## Product-polish review findings

- Resolved: Go Deeper now uses the bounded M7 selection route; `mice stop`
  cleanly requests shutdown through the owner-only bridge socket.
- Open: cloud-provider requests still put API keys in `curl` arguments. The
  planned curl-to-`ureq` migration must move authorization into HTTP headers.
- Open: Phase 4 will add explicitly granted external MCP clients (such as web
  search). AXI command-line tools remain separate, opt-in integrations so they
  cannot bypass MICE's browser-consent and sensitive-control safeguards.

## Product polish — Phase 1 (interactive overlay)

- Rebuilt the overlay result surface (`agent-macos/.../main.swift`
  `OverlayController`) from a 6-line non-scrolling `NSTextField` into a
  scrolling `NSTextView` with an action-button row; it no longer jumps to the
  mouse while already visible and uses dynamic (light/dark) colors.
- New IPC (`mice-ipc`): `OverlayResult { session_id, actions }` /
  `overlay.result` declares the buttons; the agent echoes presses back as an
  `overlay.action { sessionId, actionId }` notification.
- Selection results now offer **Go Deeper** (re-runs a deeper explanation on the
  cached selection) and **Copy**; `handle_overlay_action` + `SelectionCache` in
  `mice-cli` drive them, and `stream_selected` shares the provider streaming.
- Phase 2a — word meaning: selecting a single word / short phrase (≤3 words,
  ≤40 chars, one line) and using the summarize gesture now routes to a new
  `Action::Define` (dictionary-style: meaning, part of speech, example) instead
  of a summary; longer passages still summarize. Same gesture, intent inferred
  from length (`is_short_phrase` in `mice-cli`).
- M12 is parked pending an OpenAI key; the plan is `mice_planv6_product_polish.md`.

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
- Added M5 typed `selection.text` IPC, configurable selection shortcuts, native
  AX-first selection reading, and pasteboard-restoring Cmd-C fallback. The
  resulting summary or infographic is intentionally the next clipboard value.
- Added M6a typed prompt IPC, a portable `GoalSession` review state machine,
  strict OpenAI/Groq goal-plan schemas, goal shortcut configuration, and a
  native macOS prompt/review dialog.
- Added M6b guide-step IPC, manual guide navigation, and bounded read-only
  native AX target matching/highlighting.
- Added M6c runtime browser-step directives and extension polling. The existing
  bounded candidate-ID bridge is reused for verified selector highlights.
- Added M11a Chrome native messaging through a mode-0600 Unix socket, a
  `mice native-host` relay, and `mice setup-browser`. The extension now has a
  deterministic ID, no popup/options/token storage, and receives pushed steps.
- Added M11b Do it previews: browser steps offer Confirm/Cancel before one
  verified click; type-oriented steps accept only user-supplied transient text
  for one verified fill. Results are returned through the native bridge and
  each confirmed action is written to the terminal audit line.
- Added M11c defense in depth: Rust rejects credential/OTP/payment fills and
  authentication/payment/final-submit clicks from verified target metadata;
  the content script independently rejects the matching live DOM controls.
- Added M12: portable bounded loop state; strict OpenAI/Groq turn schemas;
  `mice autopilot`; fresh-page/navigation observation; action acknowledgement
  recovery; verified click/fill/open/scroll execution; sparse-page tab
  screenshot vision with Groq-only fallback; native narration and Esc stop;
  first-run careful-mode action confirmation.
- Repaired Chrome native-host launch: Chrome can start the executable directly,
  so MICE now detects the framed native-host invocation and relays it instead
  of exiting to usage; extension disconnects also acknowledge runtime errors.
- The resident `mice start` daemon now owns the browser socket. `mice
  autopilot` is a control client, avoiding socket theft and allowing Chrome's
  companion to remain connected between goals.
- Completed the M12 Canva-class stall fixes (2026-07-17): the extension now
  reports an empty-but-URL-bearing observation on non-injectable tabs
  (chrome://, New Tab, PDFs) so the loop escapes via `open_url` instead of
  stalling; candidate collection adds ARIA widget roles and a bounded
  cursor:pointer sweep so app UIs (Canva tiles) expose their clickable divs;
  and the vision fallback also triggers after two turns stalled on one URL,
  not only when the DOM is sparse. Verified via the standard gates and a live
  native-host connection check against the daemon socket.
- Fixed the autopilot handoff loop and blind handoff (2026-07-17): an
  `in_flight` guard collapses bursts of page observations into one turn at a
  time, terminal states tear the run down so late/duplicate observations cannot
  re-enter (stray observations are silent no-ops), and a handoff/ask_user with
  no chosen control now takes a screenshot and retries once so it can point at
  the control instead of giving up with nothing highlighted. Reloading the
  Chrome extension is required for the broadened candidate coverage to apply.
- Made the autopilot loop strictly turn-based (2026-07-17): the in_flight guard
  is now held from the start of a turn until the dispatched action's result is
  processed (released in the result handler and the ack-timeout watchdog), and
  the page-change handler ignores mutations while an action is in flight. This
  is a general fix for dynamic/SPA sites whose continuous DOM mutations
  previously caused the model to re-decide the same action repeatedly before it
  resolved.
- Consolidated autopilot narration so each turn emits one line (a single handoff
  no longer prints 2–3×), added a per-turn candidate-count diagnostic, and
  size-bounded the observation (2026-07-17): labels collapse whitespace and are
  shorter, the pointer sweep skips large containers, the guide caps are tighter,
  and a hard 12 KB observation budget keeps the highest-ranked controls that fit
  — preventing the provider HTTP 413 seen on control-dense pages like Canva.
- Made autopilot wait for single-page apps to render before snapshotting
  (2026-07-17): the extension holds a snapshot request until the DOM is briefly
  quiet or enough controls exist (capped ~2 s), so observations no longer catch
  a half-painted page (previously only skip-links on Canva). On a handoff MICE
  now always highlights — the model's chosen control, or the best-ranked
  candidate as a labelled best guess — so the user is always pointed at a target.
- Fixed the loop stalling after same-page clicks (2026-07-17): the content
  script now reports a page change only when the URL actually changes, so
  in-page panels/menus (e.g. Canva's "Create a design") trigger an immediate
  re-observation instead of waiting for a navigation event that never fires.
- Pinned a single working tab per autopilot goal (2026-07-17): the extension
  tracks one `goalTabId`, navigates it in place on `open_url` instead of
  spawning tabs, targets all snapshots/actions/highlights/screenshots at it, and
  filters page-change events to it. Fixes cross-tab confusion when the user has
  other tabs open (previously it observed the wrong tab and re-opened Canva). A
  failed browser action now reports back so the loop re-observes rather than
  stalling on a missing result.
- Made re-observation after an action unconditional (2026-07-17): a successful
  action always triggers a fresh observation (relying on the content-script
  settle wait for timing) instead of only when no navigation was reported, and
  `open_url` waits for the tab to finish loading before reporting success. Fixes
  the loop stalling after same-page interactions like Canva's "Create a design".
- Hardened content-script availability (2026-07-17): the element scan is now
  defensive so a DOM edge case can no longer abort content.js before it
  registers its message listener (which had surfaced as "Receiving end does not
  exist" / 0 controls), and background.js retries snapshot/action messages
  briefly to ride out the post-navigation injection race.
- Deduped candidates by visible label (2026-07-17): a control and its nested
  icon/text that share a label (e.g. a sidebar "Canva AI" button) no longer
  appear multiple times crowding out distinct controls or misleading the
  handoff best-guess highlight. Full autopilot pipeline now runs end to end
  (navigate → observe → act → re-observe → highlight-guided handoff); remaining
  quality gains are model judgment, best served by enabling the vision path.

## Verification

- `swift build` in `agent-macos`
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`

## Active backlog

0. M8 smart clipboard observer in `plan/mice_planv3_files_smartcopy_agents.md`;
   refine Guide follow-on UX and browser highlight presentation from real-world
   feedback.
1. Extend the M12 browser-only screenshot path to native-app ScreenCaptureKit
   vision and add multi-viewport spreadsheet reading.
2. Remove API keys from curl argument lists, preferably by moving provider HTTP
   calls to a Rust client.
3. Add input-monitoring status, correct multi-display capture, and a
   lightweight/overlay-only mode for one-shot commands.
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
- M5: in Chrome, Notes, and a PDF viewer, select text then double-tap Control;
  confirm the summary appears and replaces the clipboard only after completion.
  Select a table and press Control+Option+I; confirm the PNG infographic opens
  and is on the clipboard. Test the empty-selection hint too.
- M6a: press Control+Option+Space, enter a harmless goal, revise the generated
  plan once, then accept it. Confirm no click, keystroke, or browser action is
  performed by MICE.
- M6b: after accepting, use Back and Next through the guide. Confirm a familiar
  native button can receive a cyan best-effort highlight and that Quit ends the
  guide without acting on the target.
- M12: run `mice setup-browser`, load `browser-ext` once, then run
  `mice autopilot "search Canva and open a portrait"`. Approve the goal and,
  in first-run careful mode, each safe action. Confirm Esc stops immediately,
  `local_only` refuses, and a login/payment control becomes a handoff.
