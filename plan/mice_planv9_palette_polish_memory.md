# MICE Plan v9 — Unified palette, product polish, and personal memory

> Extends the v1–v8 plans. Planning document only: implementation lands phase
> by phase in later sessions, each phase independently green through the four
> gates (`cargo fmt --check`, `cargo clippy --workspace --all-targets --
> -D warnings`, `cargo test --workspace`, `swift build` in `agent-macos`).
> Check phases off here as they land.

## Context

MICE is feature-complete across M0–M16, but three ceilings keep it from being
10x better in daily use:

1. **Friction and speed.** Every one-shot (`mice ask`, `mice see`) cold-spawns
   a Swift agent and pays a cold Ollama model load. Features are scattered
   across ~6 gestures and 15 subcommands, and the only native text input is a
   modal `NSAlert`. The resident daemon exists but one-shots never use it.
2. **Product feel.** No menu-bar presence, no onboarding for the three macOS
   permissions, no login item, no install story beyond the DMG script.
3. **No personal memory.** `ask`/`see`/selection summaries record nothing, so
   MICE never gets personally better; the backlog's confirmation-gated
   task-planning interface still runs on modal alerts.

Three pillars answer these: **A** a Spotlight-style command palette backed by
a daemon fast path and a warm local model; **B** menu bar, onboarding, login
item, and Homebrew packaging; **C** the Goal Guide moved to panel UI plus a
local-only user history/preferences store.

## Verified current-state facts (code exploration, 2026-07-20)

- The `mice start` daemon (`main.rs:3097`) owns the Swift agent and a Unix
  bridge socket (`bridge_socket_path` `main.rs:323`; handler dispatch on
  `message["type"]` at `main.rs:413` with arms `daemon.stop`,
  `autopilot.start`, `autopilot.stop`, `bridge.hello`, `browser.*`). New
  control methods are new match arms; `mice stop` (`main.rs:1677`) is a
  working client example. The protocol is length-prefixed JSON frames.
- `ask()` (`main.rs:5377`) and `see()` (`main.rs:5500`) never touch the
  daemon: each calls `start_agent_overlay_only` → `spawn_agent`
  (`main.rs:4836`, blocking initialize handshake) and `launch_chain_pids`
  (`main.rs:4911`, up to six `/bin/ps` invocations), then opens a fresh HTTP
  call to Ollama. `ollama_chat_payload` (providers `lib.rs:380`) sends no
  `keep_alive`; nothing warms or pins the local model.
- The only native text input is `showPrompt` (`main.swift:1318`), a modal
  `NSAlert` with an `NSTextField` accessory. Guide steps are also `NSAlert`s
  (`showGuideStep` `main.swift:1364`). `OverlayController`'s main panel
  (`main.swift:1025`) is non-activating. Refined pulsing highlights exist
  (`makeHighlightPanel` `main.swift:1405`).
- No `NSStatusItem`, no `SMAppService`/login item, no launchd plist, no
  onboarding anywhere. Permissions (`MiceMacSupport/Permissions.swift`) have
  `granted`/`request()` and prompt lazily on first use.
- `memory.rs` (`SharedMemory`, `memory.rs:70+`) serves only the execution
  manager (`mice do`, `mcp-server`, `savings`); user-facing flows record
  nothing. Its patterns (root injection `at(root)` for tempdir tests,
  append-only JSONL, write lock, atomic derived files) are the template for a
  user history store.
- Gesture configuration lives in `GestureConfig` (mice-core `lib.rs:195`) and
  reaches the agent as env vars; `config_warnings` (`lib.rs:296`) validates
  it; the settings TUI (`main.rs:2888`) has 11 rows. `GoalSession`
  (`lib.rs:382`, AwaitingGoal→Planning→Reviewing→Accepted) and strict
  goal-plan schemas already exist. Overlay streaming is coalesced via
  `OverlayStream` (`main.rs:5918`). `mice see` capture excludes the launch
  chain via the `MICE_EXCLUDE_PIDS` env var set at agent spawn.

## Product decisions

- The palette becomes MICE's primary surface; `ctrl+shift+space` is
  repurposed from the capture-prompt gesture to open it (configurable
  `palette_trigger`; the old capture-prompt flow becomes palette usage).
- New `palette.*` IPC methods rather than reusing `overlay.*`, so the palette
  and the legacy overlay panel can be shown independently.
- One-shot daemon requests are not queued: a second concurrent request gets a
  typed `oneshot.busy` reply and the CLI falls back to self-spawn.
- History and preferences are local-only under Application Support, store
  only the question plus a bounded answer digest (never captures, never raw
  clipboard), and are wiped by `mice history --clear`.
- Rust stays the decision-maker for menu actions: the status item emits
  notifications; core performs shutdown/setup logic.

---

## Pillar A — unified palette + speed

### [ ] Phase 1 — Ollama warm path

- `ollama_chat_payload` (providers `lib.rs:380`): add `"keep_alive"` set from
  a new constant `OLLAMA_KEEP_ALIVE: &str = "30m"`; assert it in the existing
  payload tests (`lib.rs:703+`).
- New pure `ollama_warmup_payload(model) -> serde_json::Value`
  (non-streaming, minimal prompt, same keep_alive) with a shape test.
- Daemon startup (`main.rs:3097`): detached thread posts the warm-up request
  for the configured local model; never blocks startup; tolerates Ollama
  being down (`let _ = …`).

### [ ] Phase 2 — daemon fast path for `mice ask` / `mice see`

New bridge-socket frames (new arms beside `daemon.stop` at `main.rs:413`):

```
→ {"type":"oneshot.ask","text":"…","instruction":"…"?}
→ {"type":"oneshot.see","question":"…"?,"excludePids":[…]}
← {"type":"oneshot.chunk","text":"partial"}   (repeated)
← {"type":"oneshot.done","text":"final"}
← {"type":"oneshot.error","message":"…"}
← {"type":"oneshot.busy"}
```

- Foundation refactor first: serialize all agent-stdin writes behind one
  shared `Arc<Mutex<ChildStdin>>` so oneshot streaming cannot interleave with
  gesture-driven flows (today the daemon loop owns the stdin directly, e.g.
  `main.rs:3215`).
- Oneshot arms run the existing ask/see pipelines against the daemon's
  already-running agent, streaming into the overlay via `OverlayStream` and
  mirroring chunks over the socket for terminal printing. A `Mutex<()>`
  in-flight guard answers `oneshot.busy` instead of queueing.
- `ask()`/`see()` try `UnixStream::connect(bridge_socket_path())` first and
  fall back to the current self-spawn path on connect failure, busy, or any
  unexpected reply (version-skew safety). The fast path skips `spawn_agent`
  and the `/bin/ps` walk entirely.
- **`see` correctness:** the frame carries the *client's*
  `launch_chain_pids()` and the `screen.capture` IPC gains optional
  `excludePids` params — the daemon's own exclusion chain does not contain
  the client's terminal.
- Tests: extract pure bridge-frame parse/encode helpers and unit-test the new
  frames round-trip; no sockets in tests.

### [ ] Phase 3 — palette IPC types + verb parser (groundwork, no UI)

- mice-ipc: `PaletteSubmitted { session_id, text, front_app_name?,
  selection_text? }` and `palette.dismissed` (agent→core);
  `AgentCommand::{PaletteShow, PaletteAppendResult, PaletteFinishResult,
  PaletteDismiss}` (`palette.show`, `palette.result.append`,
  `palette.result.finish`, `palette.hide`) with wire-shape tests mirroring
  the existing ones.
- mice-core: `PaletteIntent` enum (Ask, See, Sheet, Summarize, Define, Plan,
  Tidy, File, Remember, History) + `parse_palette_intent(&str)`:
  case-insensitive leading verb, remainder trimmed, unknown/absent verb →
  `Ask(full input)`; the leading verb always wins and `ask …` escapes it.
  Full unit tests.
- `GestureConfig.palette_trigger` (default `ctrl+shift+space`), collision
  warnings in `config_warnings`, a new settings TUI row, and the default
  config template updated. Zero behavior change this phase.

### [~] Phase 4 — Swift palette panel + daemon dispatch (native panel and the
first safe command set complete; daemon fast-path and CLI-only verb dispatch remain)

- New `PalettePanel` near `OverlayController`: centered, rounded
  (`NSVisualEffectView`), ~640×72 collapsed → ~640×420 with results; large
  borderless text field, scrollable result text view (reuse the overlay
  text-view styling), dim verb-hint row.
- Activation handling (the tricky part; every existing panel is
  non-activating): subclass `NSPanel` with `canBecomeKey = true`; before
  showing, capture the frontmost app and the current AX selection (reuse the
  `selectedText()` path) for the `palette.submitted` payload;
  `NSApp.activate` + `makeKeyAndOrderFront`; Esc or dismissal restores the
  previous frontmost app so paste/selection context is not lost. The event
  tap consumes only the configured palette trigger; the palette itself
  receives keys as a normal key window.
- Gate the palette on daemon mode (`MICE_DAEMON=1` env set by the daemon's
  spawn call) so overlay-only fallback agents never open it. Auto-dismiss and
  restore focus on a dead daemon (timeout).
- Daemon dispatch on `palette.submitted`: `parse_palette_intent` →
  `Ask` → oneshot-ask internals; `See/Sheet` → the see-capture path;
  `Summarize/Define` → `handle_selection_action` (`main.rs:4236`) fed the
  payload's selection; `Plan` → Phase 5; `Tidy/File` → existing flows;
  `Remember/History` → Phase 10. Streaming via a `PaletteAppendResult`
  coalescer cloned from `OverlayStream`. `goal_trigger` (ctrl+alt+space)
  opens the palette pre-filled with `plan `.

## Pillar C — task planning & memory

### [~] Phase 5 — Goal Guide panel UI (in progress; reviewed-plan actions
and the native Guide panel are complete, palette goal entry follows Phase 4)

- `OverlayGuideStep` gains `presentation: Option<String>` (`"panel"`; absent
  keeps the legacy alert for compatibility).
- Goal entry: palette `plan <goal>` feeds `GoalSession` directly, bypassing
  `showPrompt`. Plan review renders in the result panel with
  Accept/Revise/Cancel via the existing `OverlayResult` buttons and
  `overlay.action` echoes.
- New compact non-activating `GuideStepPanel` (Step x/y, instruction,
  Next/Back/Do-it/Quit buttons) replaces the `showGuideStep` alert when
  `presentation == "panel"`; `makeHighlightPanel` highlights unchanged.
- Pure `guide_control_from_action(&str) -> Option<GuideControl>` mapper in
  mice-core with tests; `GoalSession` state tests already exist.

### [ ] Phase 9 — `UserHistory` store + `mice history`

- New type in `memory.rs` following `SharedMemory` patterns (root injection
  for tempdir tests, JSONL append, write lock, atomic writes), rooted at
  `~/Library/Application Support/MICE/history`:
  - `record(HistoryEvent { ts, kind: Ask|See|Summarize|Palette, question,
    answer_digest (≤500 chars), app_context? })`, compacting to the newest
    500 events;
  - `search(query?) -> Vec<HistoryEvent>` substring match, newest first;
  - `clear()`;
  - `remember(note)` → `preferences.json`, ≤10 notes of ≤200 chars, FIFO
    eviction; `preferences_preamble() -> Option<String>`.
- Privacy invariants (also for decisions.md): local-only, never raw captures
  or clipboard contents, only the question plus a truncated digest;
  `mice history --clear` wipes both files.
- CLI: `mice history [query]`, `mice history --clear`; usage string updated.
- Tempdir tests: retention compaction, digest truncation, preference bounds
  and eviction, search ordering, clear.

### [ ] Phase 10 — wire memory everywhere

- Best-effort `UserHistory::record` after streaming finishes in `ask()`,
  `see()`, `handle_selection_action`, and the palette dispatch (ignore IO
  errors).
- Pure `apply_preferences(instruction, preamble)` in mice-core prepends "The
  user prefers …" to ask/summarize instructions (tests: empty preamble is
  identity; no duplication).
- Palette verbs go live: `remember <note>` stores and confirms;
  `history [query]` renders results instantly with humanized timestamps — no
  model call.

## Pillar B — real-product polish

### [ ] Phase 6 — menu-bar status item

- `NSStatusItem` built only when `MICE_DAEMON=1`: live permission dots
  (refreshed in `menuWillOpen` via `MicePermission.granted`), Open Palette,
  Start at Login (disabled until Phase 8), Quit.
- New agent→core notification `menu.action { action: "openPalette" | "quit"
  | "openSetup" }` (typed in mice-ipc with a round-trip test). Core handles
  `quit` through the same shutdown path as the `daemon.stop` bridge arm; the
  agent terminates only after core sends `agent.stop`.

### [ ] Phase 7 — first-run onboarding + `mice setup`

- New `AgentCommand::OverlayOnboarding { missing: Vec<String> }`; `mice
  start` sends it when the initialize `Capabilities` report missing
  permissions.
- Swift onboarding panel (activating, non-modal): three rows with
  1-second-poll status dots; each row triggers `MicePermission.request()` and
  deep-links to System Settings
  (`x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture`
  / `Privacy_Accessibility` / `Privacy_ListenEvent`); Done hides and sends
  `onboarding.done`.
- `mice setup` subcommand re-opens it: `setup.show` bridge frame when the
  daemon runs, otherwise an overlay-only spawn.

### [ ] Phase 8 — Start at Login (SMAppService)

- `SMAppService.mainApp.register()/unregister()` behind a macOS 13
  availability guard; the menu toggle is enabled only when running from a
  real `.app` bundle, otherwise disabled with a tooltip.
- Resolve the bundle layout first: the login item launches the bundle's main
  executable (`mice`), which must run daemon mode and locate the agent beside
  itself (the sibling `agent_path()` lookup already exists). If layout work
  is needed in `scripts/package-macos.sh`, ship the toggle
  disabled-with-tooltip and record the follow-up.

### [ ] Phase 11 — Homebrew cask scaffold

- `scripts/package-macos.sh` emits `dist/mice.rb` after the DMG: version from
  the workspace `Cargo.toml`, `sha256` from the built artifact, `app
  "MICE.app"`. `--check` mode unaffected. README gains a local
  `brew install --cask ./dist/mice.rb` section; a public tap is a follow-up
  once a release repo exists.

---

## Cross-cutting risks

1. **Agent-stdin write races** between gesture flows, palette dispatch, and
   the oneshot fast path — the Phase 2 `Arc<Mutex<ChildStdin>>` refactor is
   the foundation for Phase 4; land it first.
2. **Bridge concurrency:** oneshot connections stream (long-lived) beside
   fire-and-forget autopilot frames; per-connection threads plus the single
   in-flight guard; browser frames must not starve.
3. **Version skew across the socket:** unknown `type` yields a clean error
   frame; the CLI falls back to self-spawn on anything unexpected.
4. **Activation discipline:** only the palette and onboarding panels
   activate; the event tap never consumes unconfigured input.
5. **Network-free tests:** every model-touching change is tested at the
   payload/parse layer (keep_alive payloads, verb parser, guide controls,
   history store, IPC round-trips).
6. **SMAppService bundling:** verify the packaged main-executable semantics
   before enabling the toggle.

## Docs per phase (same commit)

- `manifest.md`: capability bullets and milestone entries per phase.
- `decisions.md`: palette namespace choice; ctrl+shift+space repurpose;
  keep_alive 30m; history/preference bounds and privacy invariants;
  SMAppService degradation when unbundled; oneshot busy-reply (no queue).
- This file: check phases off as they land.

## Verification

- Every phase: the four standard gates.
- **P1:** payload tests assert `keep_alive`; with the daemon running, a
  second `mice ask` starts at warm-model latency.
- **P2:** with the daemon running, `mice ask "2+2"` spawns no new
  `mice-mac-agent` process and starts sub-second; killing the daemon yields
  identical output via the fallback path.
- **P4:** palette trigger over a text selection → type `summarize` →
  streamed result; Esc restores the previous app's focus.
- **P5:** `plan make a budget spreadsheet` → review panel with
  Accept/Revise/Cancel → step panel with working Next/Back/Quit plus the
  pulsing highlight.
- **P6/P7:** the menu-bar icon appears only under `mice start`; with missing
  permissions the onboarding panel opens and its dots flip live as grants
  are made.
- **P9/P10:** two `mice ask` runs then `mice history` lists both;
  `remember I prefer bullet answers` visibly changes the next summary;
  `mice history --clear` empties both stores.
- **P11:** `scripts/package-macos.sh` produces `dist/mice.rb` with a real
  version and sha256.
