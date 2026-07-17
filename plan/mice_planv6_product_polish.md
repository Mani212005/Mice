# MICE product polish — interactive UI, selection intelligence, MCP, M7–M10 (M12 parked)

## Context

M12 (the autonomous browser agent) is functionally working but is the hardest
piece and needs an OpenAI key for vision, so it is **parked** for now
(`plan/mice_m12_review.md` records its state). The goal of this plan is to make
MICE a better everyday product:

- Bring in and test **M7** (file-scale summarization, built in a separate
  workspace the user will merge here), then continue M8–M10.
- Replace the weak overlay UI with a proper **interactive panel** (the user
  dislikes the current one: a 6-line non-scrolling label, no buttons, jumps to
  the mouse).
- Add **selection intelligence**: word meaning ("define"), a **Go Deeper**
  button, a **Send to…** button, and eventually fetching real web links.
- Add **MCP at both ends**: MICE as an **MCP server** first (expose its cheap
  local abilities so a bigger agent like Codex offloads small repo queries to
  the local model — the token-saving pairing), then MICE as an **MCP client**
  (consume web-search/dictionary tools that power Go Deeper and link fetching).

### Verified current state (exploration, 2026-07-17)

- **Greenfield here:** M7, M8, M9, M10, MCP. No Ollama HTTP (`stream_ollama` is
  `ollama run` subprocess, `crates/mice-cli/src/main.rs:3154`), no token
  budget/chunking, no `tidy`/`file` subcommands, no HTTP client, no MCP.
- **Providers:** 9 `Command::new("curl")` sites in `mice-cli/src/main.rs`
  (guide/goal/agent-turn blocking calls + `stream_openai` :3303, `stream_groq`
  :3412, `generate_openai_image` :3362). No `reqwest`/`ureq`.
- **Selection (M5):** gesture → `sendSelectedText` (Swift `main.swift:306`) →
  `selection.text` IPC (`mice-ipc/src/lib.rs:66`, `SelectionAction` = only
  Summarize|Image) → `handle_selection_action` (`main.rs:2259`) → route → stream
  to overlay + auto-copy to clipboard (`clipboard_command`, `main.rs:2376`).
- **Overlay:** `OverlayController` (`main.swift:488`). Result panel is a
  wrapping `NSTextField`, `maximumNumberOfLines = 6` (`main.swift:504`), repositions
  to the mouse on every `overlay.show` (`main.swift:526`), no buttons/scroll.
  Interactive bits exist only as modal `NSAlert`s (`showPrompt` :603,
  `showGuideStep` :627). `AgentCommand` IPC variants are in
  `mice-ipc/src/lib.rs:152` (`OverlayShow/Update/AppendResult/FinishResult/
  ShowImage/Highlight/PromptInput/GuideStep/ClipboardSet`).
- **Actions:** `Action` enum (`mice-providers/src/lib.rs:36`) has Explain,
  Summarize, Rewrite, Translate, ExtractJson, Code, Image, Guide, GoalPlan, Qa;
  directives in `action_instruction` (`mice-core/src/lib.rs:427`). No
  web-search/dictionary anywhere.

---

## Phase 0 — Merge & test M7 (blocks nothing else; do first)

User merges the separate M7 workspace into `/Users/manijoshi/mice`. Then
reconcile against plan v3 M7 (`plan/mice_planv3_files_smartcopy_agents.md`):
Ollama HTTP `/api/chat` with `num_ctx`, per-model `input_budget_tokens`/`num_ctx`
on `ModelDescriptor` (`mice-providers/src/lib.rs:102`), a token estimator +
chunked map-reduce and a code-vs-prose heuristic in `mice-core`, and escalation
to cloud on oversized input (respecting per-local-model budgets).

**Test:** `Cmd+A` a ~1,000-line source file, Control double-tap → whole-file
summary that covers the end of the file; `local_only` shows chunk progress;
small selection stays single-shot. Gates: `cargo fmt/clippy/test`, `swift build`.

### M7 merge recipe (2026-07-17)

M7 lives in `/Users/manijoshi/mice-m7` (branch `feat/m7-file-scale-summarization`,
one commit `34d1868` on base `3ed8df7`). The current repo has **24 uncommitted
files** on the same base, so a `git merge`/`git apply` will NOT work — do a
**semantic merge** after committing the WIP checkpoint (`wip/m5-m12-phase1`).
M7 is almost entirely additive:

- `crates/mice-providers/Cargo.toml`: add `ureq = { version = "2.12",
  default-features = false, features = ["json"] }`; regenerate `Cargo.lock` via
  `cargo build` (don't hand-copy the lock).
- `crates/mice-providers/src/lib.rs`: add `input_budget_tokens: Option<usize>`
  and `num_ctx: Option<usize>` to `ModelDescriptor`, fill them across the 9
  MODELS entries (gemma3:4b 12000/16384, phi4-mini 6000/8192, gpt-oss:20b
  24000/32768, cloud entries None/None), and append the new
  `SelectionSummaryRoute`, `model_descriptor()`, `route_selection_summary()`,
  `OllamaError`, `ollama_chat_payload()`, `stream_ollama_chat()`.
- `crates/mice-core/src/lib.rs`: append `estimate_tokens`, `looks_like_code`,
  `selection_summary_instruction`, `chunk_summary_instruction`,
  `reduce_summary_instruction`, `structural_summary_chunks`,
  `summary_reduce_batches` (+ private helpers) and their tests.
- `crates/mice-cli/src/main.rs`: swap `stream_ollama`'s body to call
  `stream_ollama_chat(...)` (signature unchanged — `stream_selected` keeps
  working) and delete `AnsiStripper` (struct/impl + its two tests). Add
  `OllamaError, model_descriptor, stream_ollama_chat` to the `mice_providers`
  import.

Copy new function bodies verbatim from the M7 files; then wire selection
summarization to `route_selection_summary` + chunked map-reduce.

## Phase 1 — Interactive overlay panel (foundation for Go Deeper / Send / links)

Rebuild the result surface in `OverlayController` (`agent-macos/.../main.swift`):

- Replace the 6-line `NSTextField` with a **scrolling `NSTextView` inside an
  `NSScrollView`** so long summaries scroll instead of truncating.
- Add a **button row**: `Copy`, `Go Deeper`, `Send to…`, `Close`. Buttons post
  actions back to the core.
- **Stop jumping to the mouse:** position once when a result opens, then keep
  position for updates/streaming (the `overlay.update` path already avoids
  repositioning, `main.swift:528` — extend that policy to results).
- **Light/dark:** use dynamic `NSColor`s / `NSAppearance` so it reads in both.

**New IPC (add to `mice-ipc/src/lib.rs`, per the no-duplicated-wire-types
rule):**
- Core→agent `overlay.result { sessionId, actions:[{id,label}] }` (or extend
  `OverlayFinishResult` with an `actions` list) so the panel knows which buttons
  to show for this result.
- Agent→core notification `overlay.action { sessionId, actionId }` — the CLI
  loop (`main.rs` message loop near :2074) handles it and runs the follow-up
  (Go Deeper re-prompt, Send, etc.). Keep the streamed `appendResult` contract.

This phase is the largest Swift change and unblocks the rest of the UX.

## Phase 2 — Selection intelligence (model-only first, MCP-powered later)

- **Word meaning ("define"):** in `handle_selection_action` (`main.rs:2259`),
  when the selection is a single word / short phrase, use a new `Action::Define`
  directive ("Give a concise definition, part of speech, and one example.")
  instead of summarize. Add `Define` to `action_instruction`
  (`mice-core/src/lib.rs:427`). No new gesture needed — reuse Control double-tap
  and branch on selection length; or add `Define` to `SelectionAction`
  (`mice-ipc/src/lib.rs:84`) if a dedicated gesture is wanted.
- **Go Deeper button** (Phase 1 IPC): re-runs on the **same cached selection**
  with a deeper-explanation prompt (reuse `Action::Explain` with a "go deeper /
  more detail, background, and implications" directive), streaming into the same
  panel. Cache the last selection text per session in the CLI so Go Deeper needs
  no re-capture.
- **Send to…:** v1 targets are Clipboard (already implicit) and "paste into
  frontmost app"; later targets are MCP destinations (Phase 4) and Codex
  (Phase 3). Present a small menu from the button.
- **Fetch links:** model-only cannot fetch real URLs — defer real fetching to
  Phase 4 (MCP client). Until then, "links" is disabled or clearly labeled as
  model-suggested.

## Phase 3 — MICE as MCP server (Codex pairing; token-saving)

New subcommand `mice mcp-server` (stdio) exposing MICE's cheap local abilities
as MCP tools so a bigger agent (Codex) offloads small queries to the **local
model** (gemma3) instead of spending big-model tokens:

- Tools: `summarize_text`, `explain_code`, `define_word`, `summarize_file`
  (reuses M7 local summarization), `quick_answer`.
- Each tool routes to the local lane by default (that is the whole point);
  `local_only`-style behavior so nothing leaves the machine unless configured.
- **Transport/impl:** hand-rolled minimal **stdio MCP server** using `serde_json`
  (the project is deliberately dependency-light and already frames JSON-RPC in
  `mice-ipc`); implement the MCP `initialize` / `tools/list` / `tools/call`
  methods. (Alternative: the `rmcp` crate — heavier; note as an option.)
- Codex/other agents add MICE to their MCP config and call these tools.

## Phase 4 — MICE as MCP client (web/dictionary tools power the UI features)

MICE connects to external MCP servers the user grants (web search, dictionary,
optionally a Chrome-control server), configured in `config.toml`
(`mice-core` config). The interactive panel's **Go Deeper** and **fetch links**
call these tools; links render as clickable rows in the panel (Phase 1 UI). This
turns the deferred "fetch Google links / real definitions" into live features.

## Phase 5 — M8 / M9 / M10 (remaining file features, per plan v3)

Continue `plan/mice_planv3_files_smartcopy_agents.md`: M8 smart copy
(post-Cmd-C observer enriching real HTML/RTF; reuse `markdown_table_html` +
`ClipboardSnapshot`), M9 `mice tidy` (walkdir + `mdls` last-used + hashing;
propose→confirm, Trash-only deletes, undo log), M10 `mice file`.

## Cross-cutting — replace `curl` with a Rust HTTP client

Adopt **`ureq`** (small, blocking, fits the current sync architecture) to
replace the 9 `Command::new("curl")` sites (`mice-cli/src/main.rs:1424,1468,
1504,1540,1591,1640,3303,3362,3412`). Needed anyway for MCP/web calls; also
removes the API-key-on-argv leak (a standing backlog item). Do this alongside
Phase 3/4 when HTTP work begins.

## Sequencing

Phase 0 (merge/test M7) → Phase 1 (interactive UI) → Phase 2 (selection
intelligence, model-only) → Phase 3 (MICE MCP server / Codex pairing) → Phase 4
(MICE MCP client → live Go Deeper/links) → Phase 5 (M8–M10). The `curl→ureq`
migration lands with Phase 3/4. Order is adjustable; UI (Phase 1) should precede
the selection features that depend on it.

## Verification

- Each phase: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`, `swift build`, JS syntax checks. Keep
  provider/MCP tests network-free (mock endpoints/tools).
- **Phase 1 UI:** open a long summary → it scrolls, buttons show, panel does not
  jump to the mouse, readable in light and dark.
- **Phase 2:** select a single word → definition; press Go Deeper → deeper
  explanation streams into the same panel on the same selection; Send to…
  copies/pastes.
- **Phase 3 MCP server:** run `mice mcp-server`, drive `initialize`/`tools/list`/
  `tools/call` over stdio (or point Codex/an MCP inspector at it) and confirm a
  `summarize_file` call returns a local-model summary.
- **Phase 4 MCP client:** configure a mock web-search MCP server; Go Deeper /
  fetch-links calls it and renders clickable links in the panel.
- **M7 / M8–M10:** per their acceptance in `plan/mice_planv3_files_smartcopy_agents.md`.

## Notes / open choices (flagged, defaulted)

- MCP impl: defaulting to a **hand-rolled minimal stdio server** to stay
  dependency-light; `rmcp` is the heavier alternative if preferred.
- Overlay button back-channel: adding `overlay.action` notifications vs.
  extending `OverlayFinishResult` — defaulting to a dedicated `overlay.action`.
- M12 stays parked until an OpenAI key is available for vision.
