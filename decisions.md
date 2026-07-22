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

## Mission Control

- Mission Control treats a Markdown file under `plan/` as the reviewable
  source of work. It derives tasks conservatively and supports explicit,
  deterministic `mice:` heading metadata for task IDs, paths, and
  dependencies. The portable core rejects cycles, unsafe paths, and
  overlapping scopes that could otherwise run in parallel.
- The terminal UI is a review boundary: agent assignment, Git overlap risk,
  and base-worktree cleanliness are visible before any branch, worktree, or
  terminal is created. A red same-file overlap or a dirty base prevents launch.
- MICE creates branches and worktrees only inside owner-only application
  support storage. It retains operational lifecycle metadata only (task ID,
  agent, branch, PID, and worktree path), never provider credentials, prompts,
  agent output, captures, clipboard data, or model weights.
- Codex, Claude Code, and Antigravity CLI (`agy`) are launched through their
  documented command lines with their own permission policies intact. MICE
  never supplies a permission-bypass flag. A process exit is not success:
  the agent reports readiness from its owned worktree, and a person explicitly
  verifies Git evidence before dependent work becomes launchable.
- `mice mission watch` provides a live terminal view. When the resident
  `mice start` daemon is running, lifecycle transitions are delivered over its
  owner-only local bridge to a short native overlay; Swift renders the overlay
  and Rust remains responsible for task state.
- MICE MCP's `mission_status` exposes bounded task ownership, lifecycle, and
  overlap facts to an assigned agent without revealing another agent's checkout
  path, PID, transcript, or private configuration.

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

## M7 — file-scale selection summaries

- Local Ollama streaming uses `POST /api/chat` and NDJSON rather than a spawned
  terminal process. This avoids terminal-control-sequence leakage and lets MICE
  send a per-model `num_ctx` setting without putting selected text in process
  arguments.
- Local models declare conservative input budgets: `gemma3:4b` 12k tokens,
  `phi4-mini` 6k, and opt-in `gpt-oss:20b` 24k. Cloud models have no local
  budget because their provider limits remain provider-owned.
- Small selections preserve the existing single request. An oversized
  `local_only` selection is split at structural boundaries, summarized in
  order, then reduced locally; intermediate text stays off-screen while the
  overlay reports progress. In `cloud_allowed`, an oversized selection goes to
  the configured cloud model and MICE tells the user that route changed.
- Code receives a newcomer-oriented summary prompt and a denser token estimate;
  prose receives a general key-points summary. Content is transient and is not
  written to the repository or a runtime history.

## Product-polish follow-on fixes

- Go Deeper shares M7's selection-size routing. Large `local_only` selections
  are analyzed and reduced locally; large `cloud_allowed` selections use the
  configured cloud lane with a visible notice. This prevents a cached original
  selection from bypassing the local model's context budget.
- `mice stop` sends a shutdown request only through MICE's mode-0600 bridge
  socket. The daemon acknowledges, removes its runtime socket, and exits; the
  native child receives closed IPC and exits with it. No process-name or PID
  kill is used.

## M8 — smart copy (explicit gesture, enrich after native Cmd-C)

- Smart copy never intercepts selection, dragging, or Cmd-C itself. The user
  copies normally, then presses an explicit gesture (`smart_copy_trigger`,
  default Ctrl+Option+C); only then does the agent read the pasteboard once
  and send a typed `clipboard.captured` notification. There is no persistent
  pasteboard observer.
- Enrichment is deterministic first. Clipboard HTML containing a `<table>` —
  or plain text that is already a Markdown table — is rebuilt without any
  model into three representations: TSV as the plain-text flavor (spreadsheets
  paste it as a real grid), a semantic HTML table, and a Markdown RTF form.
  The extractor is a small quote-aware scanner in `mice-core` rather than an
  HTML-parser dependency; source apps write well-formed table markup even when
  the surrounding document is noisy.
- The model fallback (column-like text → Markdown table; other rich text →
  clean Markdown) always uses the configured local model, regardless of the
  privacy mode. Clipboard content is sensitive and never goes to a cloud
  provider; a future explicit opt-in may relax this, not a default.
- The pasteboard is rewritten only after a fully successful enrichment. A
  model failure, an unusable model response, or content with nothing to enrich
  leaves the clipboard exactly as the user's Cmd-C wrote it and says so in the
  overlay.
- Reliability hardening: clipboard notifications and writes are checked against
  the shared 16 MiB IPC frame budget before sending. A capture that cannot fit
  sends a typed `captureError` notification instead of breaking the agent/core
  connection. Oversized rich-text cleanup is chunked only on semantic source
  boundaries, never truncated; every model chunk and the joined result must
  retain the source's visible words in the same order. Unsupported smart-copy
  triggers are rejected on config load/save rather than silently doing nothing.
- Representation policy: deterministic table rebuilds expand HTML
  `colspan`/`rowspan` into explicit blank covered cells so TSV stays
  rectangular. MICE preserves an already-present PNG alongside rebuilt text
  but does not reinterpret or relabel image bytes. RTF-only and image-only
  copies are left untouched; standard generated RTF is used only after a
  successful table rebuild.
- Further safety repair: Smart Copy declines any pasteboard containing
  unsupported representations or multiple items; it never clears a clipboard
  it cannot fully restore. Link destinations are kept in the compact HTML sent
  to the local model and verified in its Markdown result. Table spans accept
  exact `colspan`/`rowspan` attributes only and have strict row, column, and
  span caps.
- Browser automation is AXI-only. The old extension engine is no longer
  selectable from `mice autopilot`; every action is confirmed individually
  with target context, then re-observed and rejected if that context changed.
  Unknown-form buttons fail closed, even when their label is neutral (such as
  “Next”), because they may submit a sensitive enclosing form.

## M9 `mice tidy` / M10 `mice file` — local file agents

- Both features are propose-then-confirm and dry-run by default. `mice tidy`
  changes nothing without `--apply`, and even then only through the review
  screen plus a final confirmation. A proposed trash candidate starts as
  *keep* in the review screen: a file reaches the Trash only when the user
  individually switches that row to trash — that switch is the per-item
  confirmation the product rules require. Deletes never bypass the Trash.
- The metadata scan needs no model: a bounded walk (2,000 files, depth 6)
  that never follows symlinks and skips hidden/system directories;
  size-then-SHA-256 duplicate detection so most files are never read; and
  Spotlight (`mdls`) last-used dates requested only for files whose
  filesystem timestamps already look stale, falling back to modified time
  when Spotlight has no answer. No new crate dependencies were added — the
  walk is hand-rolled and hashing reuses the existing `sha2`.
- The labeling and description passes are hard-wired to the local Ollama
  lane in code, not configuration: they call the local streaming client
  directly and no routing function, so file contents cannot reach a cloud
  provider in any privacy mode. Both passes are bounded (25 labels, 30
  descriptions per run, ~2 KB read per text file) and skipped gracefully
  when `ollama_model_ready` fails.
- Every applied rename is persisted to the shared undo manifest
  (`~/Library/Application Support/MICE/tidy-log.json`, atomic writes) before
  the run continues, so a crash mid-run still leaves a fully reversible log.
  `mice tidy --undo` reverses the latest run in strict LIFO order; an entry
  it cannot safely revert (missing file, occupied original path) is reported
  and kept in the log for a later retry rather than guessed at. Destinations
  are never overwritten — collisions get a ` (mice-N)` suffix — and
  cross-volume moves fail with a clear message instead of a copy-delete.
- `mice file` ranks destinations from a cached index of registered roots.
  Deterministic name-token scoring always runs; the local model, when
  available, only reorders a bounded shortlist and may answer solely with
  candidate numbers that are validated against the list — a model can never
  introduce a path of its own. The chosen move is confirmed, then recorded
  in the same undo log as tidy.
- The AXI lane-routing test now pins `MICE_QUOTA_PERCENT`: it previously
  consulted the machine's live quota reading, so a machine near its quota
  made the suite fail (and could touch the network from a test).
- M9/M10 hardening: applied filesystem actions preflight the undo log and
  serialize each read-modify-write with a stale-recoverable lock. If a log
  write nevertheless fails after a rename, MICE immediately rolls that rename
  back and reports the outcome. Scans stop traversing at the file cap rather
  than merely ceasing collection. Tidy rejects symlinked category destinations;
  filing re-canonicalizes the selected directory and requires it to remain
  within a currently registered root. `mice tidy --undo` rejects unrelated
  flags or a folder argument, and model reranking is padded with deterministic
  candidates so the chooser receives up to three options.

## Safety follow-up — browser and filesystem hardening

- Browser mutation has one authoritative route: `mice autopilot --engine axi`.
  The extension-era `autopilot.start` bridge request and Goal Guide's old
  **Do it** controls now fail closed and direct people to AXI. Goal Guide
  remains useful for planning and verified highlights, but never clicks or
  fills a page. AXI keeps generic buttons without trusted form metadata as a
  handoff until snapshot form enrichment and live coverage are available; it
  is safer to omit an ordinary click than to submit an unknown enclosing form.
- M9/M10 cannot use POSIX `rename` for a never-overwrite promise: it replaces
  an existing target after a time-of-check/time-of-use race. Moves now reserve
  a fresh path atomically with a hard link, then remove the source. That
  limits moves to regular files on the same volume and makes cross-volume work
  fail safely. Filing rejects source symlinks before canonicalization, and
  registrations serialize index updates with a stale-recoverable lock.
- Smart Copy must not present a table with an anchor rendered as ordinary
  text. Until all generated table representations can preserve anchor targets,
  a deterministic HTML table containing a link is left exactly as copied.
- Repository artifacts/macros are never cached outside a Git worktree. Git
  fingerprints include bounded regular-file state but skip symlinks, FIFOs,
  and other special files. Tidy duplicate hashing has a 512 MiB aggregate I/O
  budget. Timed-out external tools use a dedicated Unix process group and the
  caller does not join pipe readers on the timeout path, preventing a spawned
  descendant from turning the timeout into an indefinite stall.

## Native vision (`mice see`) and platform hardening

- Native capture is command-scoped, never ambient: one `screen.capture`
  request produces one capture, the captured window/display is flashed with a
  cyan frame so the person always sees what MICE looked at, and nothing is
  persisted. Credential and password-manager apps are refused by bundle-ID
  prefix before any capture happens.
- Privacy routing is structural, not configurational: in `local_only` the
  image never leaves the machine — only Apple-Vision OCR text goes to the
  local model. Cloud modes send one dimension-bounded PNG to OpenAI vision
  and fall back to the local OCR lane when no key is present.
- Multi-display correctness is one explicit conversion (`cocoaToCG`) plus
  display-under-point selection; the previous first-display assumption
  produced wrong regions on any secondary display.
- One-shot commands run an overlay-only agent (`MICE_OVERLAY_ONLY=1`) that
  never creates an event tap: `mice ask`/`mice see` need no Input Monitoring
  grant and observe no input. The result panel is held open until dismissed
  instead of dying with the process.
- Overlay streaming is coalesced (~512 bytes or 80 ms per IPC frame) because
  per-token frames could fill the agent's stdin pipe and block the provider
  stream behind a pipe write. Long results keep a bounded tail in the panel;
  the clipboard still receives the full text. RTF now escapes all non-ASCII
  as signed UTF-16 `\uN?` units (surrogate pairs for emoji).
- Config problems that degrade one feature are warnings printed at
  `mice start`/`mice doctor`, not load failures; only the smart-copy trigger
  remains a hard validation because a wrong value could consume input.

## M16 — external MCP client (Phase 4)

- Grants are explicit and doubly gated: a server must appear in
  `[[mcp.servers]]` *and* set `enabled = true`. MICE never discovers,
  auto-connects, or auto-sends content; `mice mcp call` and the Fetch Links
  button are both direct user invocations, and Fetch Links sends only a
  bounded prefix of the already-selected text.
- Server processes get a scrubbed environment (PATH/HOME/LANG/TMPDIR only),
  line-delimited JSON-RPC over stdio, a hard 20-second per-request timeout,
  and kill-on-drop. A silent or crashed server degrades to one clear error.
- Imported tools are deliberately capability-poor inside MICE: their output
  is sanitized (control sequences stripped), bounded, and rendered as text.
  There is no code path from an MCP result to the browser bridge, the tool
  registry, the clipboard writer, or any other mutation surface, and links
  are displayed, never fetched. This is the permission boundary that keeps
  an imported tool from bypassing MICE's browser-consent and privacy rules.

## Release packaging

### Safety follow-up

- `mice see` defaults strictly to the frontmost eligible window. If no window
  exists it reports that fact; it never widens a window request to a display.
  Full-display capture requires the explicit `--display` flag. The one-shot
  capture response also has a 20-second deadline; a timeout closes the agent
  rather than leaving its overlay resident.
- MCP stdio is bounded at the transport boundary: each response line is
  limited to 64 KiB before JSON parsing, tool text is appended only up to the
  display cap, and both writes and reads share the request deadline. Imported
  tool names and server-supplied errors are sanitized before any terminal or
  overlay rendering.
- Repository command stdout/stderr and Git fingerprint path lists are bounded
  while drained. A fingerprint path list that exceeds its budget disables the
  repository artifact/macro cache rather than risking memory pressure or a
  stale key. A notarization profile without a Developer ID identity is a
  configuration error, not an ad-hoc signing attempt.
- Finder filing stays user-driven: `mice file --finder` asks the macOS agent
  once for the frontmost Finder selection, requires exactly one file, and then
  uses the existing ranked and confirmed file flow. External MCP URLs are
  tappable only as standard AppKit links after the user explicitly requested
  Fetch Links; MICE does not open them itself.

- `scripts/package-macos.sh` is credential-gated rather than credential-
  dependent: without a Developer ID it produces an ad-hoc-signed
  `MICE.app` (plus DMG/zip with SHA-256 checksums) that works locally and
  gives TCC grants a stable bundle identity; setting
  `MICE_SIGNING_IDENTITY`/`MICE_NOTARY_PROFILE` upgrades the same run to
  hardened-runtime signing and `notarytool` notarization with stapling.
- The CLI resolves its agent beside its own executable first
  (`MICE.app/Contents/MacOS`), so an upgraded bundle can never pair a new
  CLI with a stale agent; the workspace debug path remains the development
  fallback. User state stays in `~/Library/Application Support/MICE`,
  making app replacement the entire upgrade procedure.

## Review fixes — Finder filing, links, MCP cleanup (2026-07-19)

- `mice file --finder` no longer requires Finder to be frontmost: invoking
  the CLI necessarily makes the terminal frontmost, so that check rejected
  every normal use. Finder must merely be running; the confirmation prompt
  still names the exact file before anything moves.
- Finder paths are forwarded exactly as reported. Trailing whitespace and
  newlines are legal in macOS filenames, and trimming them could silently
  move a *different* existing file with the trimmed name.
- The Finder capture has a 60-second deadline (generous because the first
  use can show an Automation permission prompt) and the CLI enforces the
  exactly-one-path protocol — a malformed or incompatible agent response is
  an error, never a guess at the first path.
- Result-panel links are attributed by MICE itself and restricted to
  HTTP/HTTPS. Foundation's automatic detection was wrong for this surface:
  it also linkifies file:, mailto:, and custom URL schemes.
- MCP servers run as their own process-group leaders and cleanup kills the
  group, so a shell-spawned descendant can no longer outlive the timeout,
  hold the stdio pipes, and keep the detached reader thread blocked. A
  regression test spawns a real grandchild and asserts it dies.

## M14 — safe form-context enrichment for generic buttons

- A button with no `form=` metadata of its own now inherits a page-level
  form context derived read-only from the same AXI snapshot: if the snapshot
  shows at least one enumerable input and every visible input is positively
  safe (trusted text-like type, no sensitive/code-like label, no sensitive
  autocomplete), a neutral button click is allowed after the usual
  confirmation. A page with any sensitive-looking input blocks with a
  sensitive-page reason, and a snapshot with no visible inputs proves
  nothing and stays fail-closed exactly as before. Sensitive click labels
  (pay, sign in, submit…) remain blocked regardless of page context.

## Native vision — `--sheet` multi-viewport reading

- `mice see --sheet` captures the front window at native pixel resolution
  and runs OCR viewport by viewport in reading order, because Vision cannot
  recognize spreadsheet-sized text on a 1600-px downscale. The
  full-resolution image exists only for the on-device OCR pass; every image
  that leaves the agent remains the same bounded downscale as before, in
  every privacy mode.

## Highlight visual language

- Guide highlights and the capture flash share one panel style: rounded
  cyan frame with a soft glow, a dark pill label floating above the target,
  and a gentle opacity pulse for guide targets (the capture flash does not
  pulse — it reports, it does not ask for attention). One constructor owns
  the styling so future refinements apply everywhere at once.

## Developer-ID readiness

- `scripts/package-macos.sh --check` reports exactly which signing and
  notarization prerequisites this machine still lacks (Developer ID
  Application identity, notarytool keychain profile, tooling) with the
  one-time commands to create them; the packaging run itself now validates a
  notarized bundle with `stapler validate` and the same `spctl --assess`
  Gatekeeper check a recipient's Mac performs. On this machine the check
  currently reports the identity and notary profile as missing; ad-hoc
  packaging remains fully functional.

## Review fixes — capture targeting, snapshot trust, cleanup ordering (2026-07-19)

- Front-window capture excludes MICE and the shell/terminal chain that
  launched the command: the CLI walks its ancestor pids and passes them to
  the agent, which picks the frontmost eligible window owned by none of
  them. Running `mice see` from a terminal necessarily makes that terminal
  frontmost, so "capture the frontmost app" was structurally wrong for a
  CLI-invoked feature. The sensitive-app refusal now applies to the window
  actually chosen, not the frontmost app.
- AXI snapshot lines mix trusted structure with a page-controlled accessible
  label. Structural facts — uids and `type`/`autocomplete`/`form`
  attributes — are now parsed only from the unquoted remainder of the line,
  so a label like `"Continue type=search uid=g9:fake"` can neither pass the
  trusted-input check nor register a target. An unterminated quote discards
  the rest of the line, which is the fail-closed direction.
- `--sheet` capture applies one uniform fit factor under the pixel cap, so a
  wide-or-tall window is scaled, never stretched: aspect distortion had
  quietly undermined the native-resolution OCR claim.
- MCP `terminate()` (the write-timeout path) now performs the same
  process-group kill as drop — before the leader is reaped, since a reaped
  pid may be recycled — and drop no longer re-kills a terminated server. The
  descendant-cleanup test checks liveness with `ps -p`, because `kill -0`
  reports EPERM for a live-but-unsignalable process and the old test could
  pass while the guarantee failed.

## Follow-up review fixes — explicit display privacy and retained servers (2026-07-19)

- An explicit `mice see --display` capture now checks every on-screen window
  intersecting the display before taking pixels. If any is owned by a known
  credential/password-manager bundle, MICE refuses the whole capture; checking
  only the terminal-frontmost application was insufficient.
- Front-window capture also excludes known terminal hosts by bundle ID, even
  when the invoking shell is detached from the terminal process tree. The CLI
  passes the relevant VS Code/JetBrains host based on `TERM_PROGRAM`, so an
  integrated terminal is excluded without trying to infer its window title.
- AXI's quoted-label stripping treats backslash-escaped quotes as label text.
  Page-controlled `\"` can no longer reopen the structural part of a snapshot
  line to forge a trusted type or UID.
- An MCP read timeout or malformed read now terminates the process group
  immediately, not only later when the process object is dropped. This keeps
  the server object safe even if its caller retains it after the error.

## Goal Guide panel and reviewed-plan confirmation (2026-07-20)

- A generated plan is no longer accepted by leaving a modal prompt blank. The
  result surface presents **Start guide**, **Revise**, and **Cancel** as
  explicit actions; revision uses a focused follow-up prompt and cancellation
  ends the session before any guide state is created.
- `OverlayGuideStep.presentation = "panel"` is an opt-in shared IPC field.
  Older agents retain the alert fallback, while current macOS agents use a
  rounded non-activating panel. It keeps the foreground app usable and sends
  only an explicit Where?/Back/Next/Quit decision to the Rust core.
- Guide highlights use a restrained macOS-style blue → violet → pink → amber
  gradient with rounded corners. The color draws attention, not authority:
  MICE still only reads and highlights targets; it never clicks, types, or
  submits from Goal Guide.

## Palette and history privacy follow-up (2026-07-20)

- The palette shortcut is a palette-only gesture, not a screen-capture alias.
  Palette activation reads a selection once, after the explicit shortcut, then
  sends only that one submitted request to the core.
- Personal history stores an event label for selection summaries and a bounded
  application name for `mice see`; it never records selected source text,
  clipboard content, pixels, or a window/document title. Goal plan/session
  data is likewise removed from the daemon as soon as the guide ends.
- Palette requests use small byte-bounded IPC inputs and a session-bound,
  coalesced 12,000-character output stream. Late provider output is ignored by
  a newly opened palette rather than being attached to the wrong request.
- `define term` is an explicit intent: its typed term wins over any selection
  and the core uses the typed `Define` action instead of summary heuristics.
  The Goal gesture is a daemon-only shorthand for opening the palette with
  `plan ` already entered.

## Desktop launcher, private setup, and harness integration (2026-07-20)

- The macOS product installs as a self-contained `MICE.app` in the user's
  Applications directory and exposes a `~/.local/bin/mice` launcher. Bare
  `mice` is the friendly non-blocking entry point; `mice start` remains the
  foreground diagnostic path so terminal troubleshooting stays available.
- Local Only setup may start Ollama automatically. It reuses an already
  running service and records a PID only for a server MICE itself spawned, so
  `mice stop` cannot terminate another app's Ollama instance. Automatic model
  download is intentionally limited to `gemma3:4b` with an 8 GiB free-space
  guard; larger or alternate models remain explicit choices.
- Codex and Claude Code are connected through the existing stdio
  `mice mcp-server`, never a cloud bridge. `mice connect` is user-scoped and
  confirmation-gated, uses each harness's own MCP command, and refuses to
  overwrite an existing `mice` entry automatically.

## Local Goal Guide reliability and plan recall (2026-07-20)

- Goal Guide uses the routed local Ollama model in Local Only mode. Its parser
  accepts a complete JSON object inside a Markdown code fence because the
  configured `gemma3:4b` emits exactly that otherwise-valid form. A reachable
  local model whose output remains malformed gets a small advisory starter
  plan; a missing Ollama server or model remains an explicit error and never
  becomes a pretend-success.
- A typed goal and its bounded advisory plan are intentional, owner-only local
  memory. They are stored as `goal_plan` history events, surfaced as the two
  most recent goals in MICE Home, and fully reviewable with `mice plans`.
  This does not relax the privacy rule for captures, clipboard data, or text
  selections, which still are never stored as source material.
- MICE Home can run as a display-only helper beside the resident daemon. Its
  **Plan a goal** control therefore replays the daemon's configured Goal
  gesture instead of sending IPC into its own unread stdout. That lets the
  control reliably open or resume the actual plan session.

## Provider credentials in macOS Keychain (2026-07-20)

- Cloud keys are a user-owned operating-system secret, not configuration.
  `mice keys set groq` and `mice keys set openai` collect visible normal-line
  terminal input and send it to `/usr/bin/security` over stdin. Raw hidden
  input was deliberately removed because bracketed-paste markers could become
  part of a pasted key. No key reaches the TOML file, shell history,
  repository, or a command argument. Runtime provider lookup
  prefers an explicit environment override, then the login Keychain, so the
  resident MICE app can use a saved key even when launched from Finder.

## Settings must describe routing, not just configuration (2026-07-20)

- `cloud_allowed` is not synonymous with “everything uses cloud”: routine
  text, hover, and selection work stays on the configured local model, while
  Goal Guide, browser, and image-capable work use the configured cloud lane.
  The settings TUI now renders that active outcome alongside one-time local
  model and provider-key availability checks, so a missing Groq key is visible
  before a Goal Guide request fails.

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
