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
| M4 packaging and Linux preparation | macOS packaging pipeline complete (`scripts/package-macos.sh`: app bundle, ad-hoc or Developer-ID signing, credential-gated notarization, DMG/zip). Linux handshake scaffold exists; desktop implementation deferred. |
| M5 selection actions (summarize / infographic gestures) | Implemented; manual acceptance remains. |
| M6 Goal Guide (goal popup → plan → step-by-step guidance) | M6a–M6c implemented; browser transport is superseded by M11a native messaging. |
| M11 Guide-me that acts | Superseded for mutation: Goal Guide is highlight/explain-only; AXI is the sole confirmed browser-action path. |
| M12 Web Autopilot & Companion | Implemented; **parked** pending an OpenAI key for vision. See `plan/mice_m12_review.md`. |
| M13/M15 execution manager | Implemented: deterministic CLI registry, local tool loop, MCP delegation surface, shared memory/artifact cache, workflow macros, capability advertisement, and savings ledger. |
| M14 AXI browser guide | Review-first implementation: AXI observes, proposes one action with target context, re-observes after confirmation, validates the current UID, then acts. Generic buttons use safe snapshot-derived form-context enrichment: a click is allowed only when every visible input is positively safe; sensitive or unknown pages still hand off. Structural parsing (uids, type/form/autocomplete attributes) reads only the unquoted portion of each snapshot line, so page-controlled accessible labels cannot forge safe input types or targets. |
| Product polish (interactive UI, selection intelligence, MCP, M7–M10) | Phases 1, 2a, 3 (local MCP server), M7 file-scale summarization, M8 smart copy, M9 `mice tidy`, and M10 `mice file` are complete; manual acceptance for M8–M10 remains. |
| M16 MCP client (Phase 4) | Implemented: user-granted stdio servers, scrubbed environment, timeouts, `mice mcp list/call`, overlay Fetch Links; imported tools are text-only and cannot reach mutation surfaces. |
| Native vision & platform hardening | Implemented: `mice see` window/display capture with OCR/vision privacy routing, multi-display fixes, overlay-only one-shot mode, Input Monitoring status, stream backpressure, Unicode RTF, config warnings, long-result trimming. |

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
- **M8 smart copy:** after a normal Cmd-C, **Control+Option+C** (configurable
  `smart_copy_trigger`, settings row included) asks MICE to enrich the copied
  content. The agent reads the pasteboard once — never continuously — and
  sends a typed `clipboard.captured` notification. Copied HTML tables and
  plain Markdown tables rebuild deterministically, with no model call, into
  TSV plain text (real grids in Numbers/Sheets), a semantic HTML table, and a
  Markdown RTF form. Column-like text and other rich text fall back to the
  configured local model only — clipboard content never routes to a cloud
  provider regardless of privacy mode. The IPC payload and rewritten clipboard
  are frame-bounded: an over-limit capture reports a typed error without
  disconnecting MICE, and an over-limit result leaves the original clipboard
  intact. Local model work is split only at lossless semantic boundaries and
  must preserve every visible token in order before a rewrite is accepted.
  Tables expand `colspan`/`rowspan` into a rectangular grid; an existing PNG
  remains independent and is never interpreted. If a copy contains TIFF,
  file URLs, custom formats, multiple pasteboard items, or a PNG too large to
  preserve, MICE declines enrichment rather than clearing any representation.
  Rich-text cleanup preserves and verifies link destinations. Supported
  triggers are explicitly validated as `ctrl+alt+c` or `ctrl+alt+x`. A linked
  HTML table is deliberately left untouched until MICE can preserve its anchor
  destinations in every generated table representation.
- **M9 `mice tidy`:** a privacy-first folder organizer, dry-run by default.
  The metadata scan uses no model: a bounded walk (2,000 files, depth 6,
  symlinks never followed, hidden/system directories skipped),
  size-then-SHA-256 duplicate sets (with a 512 MiB aggregate hashing budget), and Spotlight last-used dates only for
  files whose filesystem timestamps already look stale. It prints the
  headline report ("N files; M unopened >6 months; K duplicate sets") and the
  proposed keep/move/trash actions. `--apply` opens a review screen where
  trash suggestions start as *keep* — a file is trashed only when its row is
  individually switched to trash — and a final confirmation applies the run.
  A bounded local-model labeling pass (≤25 files, ≤2 KB each, skipped with
  `--no-label` or when Ollama is down) annotates the review list; file
  contents never reach a cloud provider in any privacy mode, enforced in
  code. Deletes only ever move files to the Trash, nothing is overwritten,
  the undo log is preflighted before a rename, and every applied rename is
  persisted under a lock (or immediately rolled back if that write fails).
  `mice tidy --undo` reverses the last run (unrevertable entries are kept for
  a later retry). Moves reserve the destination with an atomic hard link
  before removing the source, so neither a collision nor a TOCTOU race can
  replace an existing file; cross-volume moves fail safely.
- **M10 `mice file`:** smart filing into registered roots.
  `mice file --add-root ~/github` indexes a root's visible subfolders (two
  levels, bounded) with cached one-line local-model descriptions from a
  README excerpt or entry listing when Ollama is available. `mice file
  <path>` ranks the top three destinations — deterministic name-token scoring
  always, with the local model reordering a bounded shortlist by candidate
  number only (validated; a model can never introduce its own path) — then
  asks the user to pick and confirm before one safe, never-overwriting move
  that is recorded in the shared tidy undo log. The selected destination is
  revalidated as a real directory within a registered root immediately before
  moving. The source must be a regular non-symlink file, and concurrent root
  registrations use a stale-recoverable index lock. `mice file --finder`
  reads one explicitly selected Finder file through the macOS agent, then
  follows the same ranking and confirmation flow; it never observes Finder
  continuously or moves a file without confirmation. Finder does not need to
  be frontmost (running the command necessarily makes the terminal frontmost;
  the confirmation prompt still names the exact file). Paths are forwarded
  exactly as Finder reports them — filenames legally ending in whitespace or
  newlines are never trimmed into a different file. The CLI enforces the
  exactly-one-path protocol and applies a 60-second deadline so an
  Automation-permission prompt or stuck agent cannot hang the command.
- **Native vision (`mice see`):** `mice see [--display|--sheet] "<question>"`
  answers a question about the user's own screen. The default captures the
  frontmost eligible window *excluding MICE and the shell/terminal chain that
  launched the command* (the CLI passes its ancestor pids to the agent), so
  the capture reads the app the person is asking about rather than the
  terminal that is necessarily frontmost; the sensitive-app refusal applies
  to the window actually being captured. No eligible window is a refusal,
  never an implicit display capture. `--sheet` reads dense small text (spreadsheets):
  the window is captured at native pixel resolution (uniformly scaled under
  one cap, so the aspect ratio is never distorted) and OCR runs viewport by
  viewport in reading order, while the image sent to any model remains the
  same bounded downscale — full-resolution pixels never leave the machine.
  `--display` explicitly captures the display under
  the mouse via ScreenCaptureKit with correct
  Cocoa↔CG multi-display coordinate mapping, flashes a cyan frame over
  exactly what it captured, refuses a display when any visible
  credential/password-manager window is present (not merely when that app is
  frontmost), and never persists a capture. `local_only` sends only on-device
  OCR text to the local model — pixels never leave the machine; cloud modes
  send one bounded PNG to OpenAI vision when a key is present and fall back
  to the local OCR lane otherwise. The typed `screen.capture`/`screen.captured`
  IPC reports refusals as data instead of breaking the stream, and a
  20-second capture deadline stops a hung one-shot agent safely.
- **M16 MCP client:** `[[mcp.servers]]` entries with `enabled = true` are the
  only external MCP servers MICE will spawn. Servers run with a scrubbed
  environment (PATH/HOME/LANG/TMPDIR only — provider keys can never leak),
  line-delimited JSON-RPC over stdio, a hard per-request read/write timeout,
  64 KiB pre-parse line limit, and kill-on-drop. `mice mcp list` discovers tools; `mice mcp call` invokes one
  explicitly. When a granted server exists, results offer a **Fetch Links**
  button that queries the first search-style tool with a bounded prefix of
  the selection. Imported tools surface only as sanitized, bounded text:
  they have no route into MICE's browser bridge, tool registry, clipboard,
  or any mutation surface. MICE applies link attribution itself, restricted
  to HTTP/HTTPS URLs (automatic AppKit detection is disabled because it also
  linkifies file:, mailto:, and custom schemes); a link opens only after the
  person clicks it. Timeout/drop cleanup kills the server's whole process
  group immediately on either a read or write failure, so shell-spawned
  descendants cannot keep the stdio pipes and reader thread alive.
- **Platform hardening:** `mice status` reports Input Monitoring alongside
  the other capabilities. One-shot commands (`mice ask`, `mice see`) use an
  overlay-only agent mode that creates no event tap — no Input Monitoring
  grant needed, no input observed — and hold the result panel open until
  dismissed. Region capture follows the mouse to the correct display.
  Overlay streaming is coalesced (~512 B / 80 ms batches) so a fast provider
  can no longer stall behind the agent's stdin pipe; very long results keep
  a bounded live tail in the panel with the full text still on the
  clipboard. RTF output escapes non-ASCII as UTF-16 `\uN?` units so accents
  and emoji survive rich-text pastes. `mice start`/`mice doctor` print
  non-fatal config warnings (unknown models, unsupported triggers,
  out-of-range timings, malformed MCP entries).
- **Packaging:** `scripts/package-macos.sh` builds release binaries, wraps
  them in `MICE.app` (agent beside the CLI; the CLI prefers its sibling
  agent so upgrades never mix versions), ad-hoc signs for local use, and
  produces a checksummed zip and DMG. Developer-ID signing (hardened
  runtime + entitlements) and notarization activate automatically when
  `MICE_SIGNING_IDENTITY` / `MICE_NOTARY_PROFILE` are set; a notary profile
  specifically requires a named `Developer ID Application:` identity, so an
  ad-hoc/development-signed bundle is never submitted. User state lives
  in `~/Library/Application Support/MICE` and survives app replacement.
- **Phase 3 local MCP server:** `mice mcp-server` provides stdio JSON-RPC MCP
  tools for `summarize_text`, `summarize_file`, `explain_code`, `define_word`,
  and `quick_answer`. These use only the configured local Ollama model; MICE
  never routes MCP tool text to a cloud provider. Large local summaries reuse
  M7's structural chunk-and-reduce flow.
- **Execution manager (M13/M15, v8 multipliers):** `mice tools` exposes a
  deterministic-first registry for Git, repository search, GitHub (`gh-axi`
  with `gh` fallback), Chrome AXI, and quota inspection. Repository-state
  results have a bounded return contract with an `artifact:<key>` reference;
  live browser, quota, and remote results deliberately have no persistent
  reference. Read-only repository results cache by tool arguments plus
  repository state only inside Git worktrees; non-Git directories never reuse
  a repository artifact or workflow macro. Git fingerprints skip symlinks and
  special files and bound untracked-file reads. `mice do` runs bounded local tool
  loops on capable machines, while `mice mcp-server` exposes `run_tool`,
  `delegate_task`, `git_summary`, `repo_grep`, `memory_note`, `memory_query`,
  and `team_status` to every MCP-compatible harness. The shared file-backed
  memory store records bi-temporal events, derived facts/digests, artifacts,
  macro workflows, overlap warnings, and the `mice savings` ledger. Tool
  subprocesses receive a scrubbed environment without provider API keys and,
  on timeout, their process group is killed without waiting forever for a
  descendant-held output pipe.
- **M13/M14 safety (2026-07-18):** raw browser mutations remain unavailable
  to MCP and generic tool loops. `mice autopilot --engine axi <goal>` is the
  only supported mutation path; the legacy extension engine is retired. AXI
  observes through AXI, shows one proposed action with the target's current
  label/context,
  requires a human confirmation, re-observes, validates the exact current UID,
  and rejects a changed target context before invoking that one action. AXI
  fills additionally require a trusted safe input type in the current snapshot;
  opaque, password/code/OTP/payment, submit/confirm, sign-in, transfer,
  file-return, untrusted-form buttons, and unknown-form Enter actions fail
  closed. The legacy native-bridge start message and Goal Guide's former
  **Do it** action fail closed with an AXI handoff, so an old extension cannot
  bypass those checks. A generic button without trusted form context also
  hands off pending AXI form-enrichment coverage. A stale target gets one safe re-observe/replan attempt;
  a repeat or Chrome loss pauses with the last safe history rather than acting.
  Browser snapshots, quota, and remote GitHub results are never
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
- Resolved: all OpenAI and Groq provider paths now use in-process `ureq`
  requests. Authorization stays in HTTP headers rather than `curl` arguments,
  so provider API keys are not visible through process listings.
- Open: Phase 4 will add explicitly granted external MCP clients (such as web
  search). AXI command-line tools remain separate, opt-in integrations so they
  cannot bypass MICE's browser-consent and sensitive-control safeguards.
- Resolved: deterministic tool subprocesses have a 45-second kill timeout;
  `repo.grep` inserts `--` before its pattern and permits only relative paths
  within the current repository. Cache and macro keys include the canonical
  repository path and content-sensitive worktree fingerprint, so dirty edits
  and different repositories cannot reuse each other's result. Stale shared
  memory locks are reclaimed after two minutes and malformed/torn JSONL lines
  are skipped during recovery.
- Resolved: local AXI tool-loop selection verifies both a reachable Ollama
  server and the configured installed model through `/api/tags`; quota routing
  reads quota-axi JSON (or explicit `MICE_QUOTA_PERCENT`) once per five-minute
  window when available.

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

0. Tune the refreshed highlight visuals (pulse, pill labels) and Guide
   follow-on UX from real-world feedback.
1. Build the confirmation-gated task-planning interface (the non-persistent
   post-Cmd-C clipboard capture landed with M8 smart copy).
2. Run the signed/notarized packaging path once a Developer ID is available:
   `scripts/package-macos.sh --check` reports the exact prerequisites still
   missing; the same script then signs, notarizes, staples, and validates
   (`stapler validate`, `spctl --assess`). Defer PipeWire/portal/AT-SPI/libei
   implementation until Apple refinement is complete.

## Manual acceptance still useful

- M2: request an infographic from a selected table and paste into spreadsheet
  and rich-text destinations.
- M3: test a Control-hover explanation and a browser guide request on an
  unfamiliar control.
- M5: in Chrome, Notes, and a PDF viewer, select text then double-tap Control;
  confirm the summary appears and replaces the clipboard only after completion.
  Select a table and press Control+Option+I; confirm the PNG infographic opens
  and is on the clipboard. Test the empty-selection hint too.
- M8: copy a styled table from a Chrome page, press Control+Option+C, paste
  into Numbers/Google Sheets (expect a real grid via TSV) and into Notes
  (expect a clean table via HTML). Include a merged-cell table to confirm its
  rectangular geometry is preserved. Copy plain prose and confirm the "left
  as copied" notice with the clipboard untouched; stop Ollama and confirm a
  tabular-text enrichment fails without altering the clipboard. Copy a very
  large document and confirm MICE reports the safe size limit without losing
  its connection or changing the original clipboard.
- `mice see`: with two displays, run `mice see --display "what is on screen?"`
  on each display and confirm the cyan flash frames the correct one; run
  `mice see` over a password manager window and confirm the refusal; in
  `local_only`, confirm the answer cites OCR text and no network vision call
  is made. Open a dense spreadsheet and compare `mice see` with
  `mice see --sheet "what is in column C?"` — the sheet mode should read
  small cell text the default mode misses.
- `mice file --finder`: select a file in Finder, switch to the terminal, and
  confirm the command still reads the selection (Finder need not be
  frontmost); create a file whose name ends in a space and confirm it files
  under its exact name; deny the Automation prompt once and confirm the
  command errors out within 60 seconds instead of hanging.
- M14 enrichment: on a page with only a search box, confirm a neutral
  "Next"-style button is now clickable after confirmation; on a page with a
  code/OTP field, confirm the same button still hands off.
- M16: grant a real web-search MCP server, select text, press Fetch Links,
  and confirm only http(s) URLs become clickable (a file: or mailto: string
  stays plain text); verify `ps e` on the server process shows no provider
  API keys, and that killing a hung server also removes its shell
  descendants.
- Packaging: install `dist/MICE.app` on a second Mac, grant permissions to
  the app, and verify gestures work and an app-bundle replacement keeps
  config, undo log, and filing index.
- M9: run `mice tidy` on a disposable folder seeded with old and duplicate
  files; verify the dry-run report, that `--apply`'s review screen requires
  individually switching a row to trash, that applied files land in category
  folders and `~/.Trash`, and that `mice tidy --undo` restores everything.
- M10: register two project roots, file a PDF and a code file, verify the
  top-3 proposals are sane (with Ollama running, descriptions and ranking use
  the local model; without it, name matching), and that `mice tidy --undo`
  restores a filed move.
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
