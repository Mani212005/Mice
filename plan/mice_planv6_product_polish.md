# MICE product polish — remaining work (planner/reviewer view, 2026-07-17)

## Context

M12 (autonomous browser agent) is parked pending an OpenAI key for vision
(`plan/mice_m12_review.md`). The active goal is to make MICE a better everyday
product. This plan is the **remaining work** after the M7 merge; it is written
for a separate implementing agent. Save location: this file, in the repo
`plan/` folder.

## Current state (verified in the merged `main`, all gates green)

Done:
- **M0–M6, M11, M12** (M12 parked).
- **M7 — file-scale summarization** (merged, `a3ab1b6`): Ollama now uses the
  HTTP `/api/chat` API with per-model `num_ctx` (`stream_ollama_chat`,
  `mice-providers`); `ModelDescriptor` gained `input_budget_tokens`/`num_ctx`;
  `mice-core` has `estimate_tokens`, `looks_like_code`,
  `selection_summary_instruction`, `structural_summary_chunks`,
  `summary_reduce_batches`; selection summaries route through
  `route_selection_summary` and chunk-map-reduce oversized local-only inputs via
  `stream_chunked_selection_summary` (`mice-cli/src/main.rs:2376`), with a
  cloud-escalation notice. `ureq` is now a workspace dependency
  (`crates/mice-providers/Cargo.toml`).
- **Phase 1 — interactive overlay** (`agent-macos/.../main.swift`
  `OverlayController`): scrolling `NSTextView`, a Copy/Go Deeper button row,
  no mouse-jump while visible, light/dark. New IPC `OverlayResult`/
  `overlay.result` + agent→core `overlay.action`; CLI `SelectionCache`,
  `handle_overlay_action`, `run_go_deeper`, `stream_selected`
  (`mice-cli/src/main.rs:2296+`).
- **Phase 2a — word meaning**: `Action::Define` + `is_short_phrase`
  auto-detection routes a single word / short phrase to a dictionary-style
  answer on the same summarize gesture.

Not built (this plan): Phase 3 (MICE MCP server), Phase 4 (MICE MCP client),
Phase 5 (M8–M10), and the curl→ureq migration. Phase 2b and the bounded Go
Deeper / `mice stop` review fixes are complete.

## Review findings to fold in

1. **Resolved — Go Deeper local-context overflow.**
   `run_go_deeper` (`mice-cli/src/main.rs:2472`) runs a single-shot
   `Action::Explain` on the full cached text. It now reuses
   `route_selection_summary` and local chunk-map-reduce, preserving the Go
   Deeper prompt for the final response.
2. **API keys still passed on the curl argv.** 9 `Command::new("curl")` sites in
   `mice-cli/src/main.rs` put `Authorization: Bearer <key>` in argv (visible via
   `ps`). The `ureq` dep from M7 makes the fix cheap — see the cross-cutting
   task.
3. **Resolved — `mice stop`.** The subcommand connects to the owner-only bridge
   socket, sends a shutdown control frame, waits for acknowledgement, and lets
   the daemon close its native-agent IPC cleanly.

---

## Phase 2b — "Send to…" button

**Complete (v1):** result actions now include **Send to…**, whose native menu
offers **Paste into frontmost app**. It reuses the rich text/HTML/RTF clipboard
already set for the result and synthesizes a normal Command-V; the overlay is
non-activating, and MICE reactivates the destination captured when the result
opened before pasting. Richer MCP and Codex destinations remain deferred to
Phases 3–4.

Add a `Send to…` action to the result panel (`selection_result_actions` in
`mice-cli` + the button row in `OverlayController`). v1 destinations, chosen
from a small native menu (`NSMenu` off the button):
- **Copy** (already implicit) and **Paste into the frontmost app** (inject the
  result via the existing AX/CGEvent path in `MiceMacSupport`), so the user can
  drop a summary straight into their document.
- Stub the richer destinations (MCP targets, Codex) until Phase 3/4.

Wire: the button posts `overlay.action { actionId: "send_paste" }`; the CLI
`handle_overlay_action` sends a `text.inject`-style command (add the IPC command
if absent) with the cached response.

## Phase 3 — MICE as an MCP server (Codex pairing; token-saving)

New subcommand `mice mcp-server` (stdio) exposing MICE's cheap local abilities
as MCP tools so a bigger agent (Codex) offloads small queries to the **local
model** instead of spending big-model tokens.

- **Tools:** `summarize_text`, `explain_code`, `define_word`, `summarize_file`
  (reuse M7 `route_selection_summary` + chunk-map-reduce for large files),
  `quick_answer`. Each routes to the local lane by default.
- **Impl:** hand-rolled minimal stdio MCP server using `serde_json` (the project
  is dependency-light and already frames JSON-RPC in `mice-ipc`). Implement MCP
  `initialize`, `tools/list`, `tools/call`. Put the tool logic in a new
  `mice-mcp` module/crate or inside `mice-cli`; reuse `mice-core`/`mice-providers`
  summarization directly (no HTTP to self).
- Codex/other agents add MICE to their MCP config and call these tools.

## Phase 4 — MICE as an MCP client (web/dictionary → live features)

MICE connects (stdio) to external MCP servers the user grants — web search,
dictionary, optionally a Chrome-control server — configured in `config.toml`
(add an `[mcp]` section + list of servers to `mice-core` `Config`). The panel's
**Go Deeper** and a new **Fetch links** action call these tools; links render as
clickable rows (extend `OverlayController`/`overlay.result` to carry link
items). This turns the deferred "real definitions / Google links" into live
features.

## Phase 5 — M8 / M9 / M10 (file features, per plan v3)

Per `plan/mice_planv3_files_smartcopy_agents.md`:
- **M8 smart copy:** post-Cmd-C observer enriching the real HTML/RTF the app
  wrote — reuse `markdown_table_html` (`mice-core`) + the `ClipboardSnapshot`
  read/restore (`agent-macos/.../main.swift`); add a `smart_copy` trigger.
  Deterministic table → TSV/HTML first; local LLM only for messy cases.
- **M9 `mice tidy <folder>`:** `walkdir` scan + `mdls` last-used + size-then-hash
  dedupe → local-LLM labels (bounded) → propose→confirm; Trash-only deletes;
  undo log at `~/Library/Application Support/MICE/tidy-log.json`.
- **M10 `mice file <path>`:** registered project roots + local index → local-LLM
  top-3 destinations → confirm → move (shared undo log).

## Cross-cutting — replace curl with `ureq`

`ureq` is already a dependency (M7). Migrate the 9 `Command::new("curl")` sites
in `mice-cli/src/main.rs` (OpenAI/Groq guide/goal/agent-turn/stream + image) to
`ureq`, moving `Authorization` into headers (fixes review #2) and removing the
runtime `curl` dependency. Keep streaming behavior (SSE) via `ureq`'s reader.
Do this before/with Phase 3–4 (MCP and web tools want a real HTTP client).

## Sequencing

Review fixes (#1 Go-Deeper budget, #3 `mice stop`) are quick and can land first.
Then Phase 2b → curl→ureq migration → Phase 3 (MCP server) → Phase 4 (MCP
client) → Phase 5 (M8–M10). Order is adjustable; the interactive panel (done)
already unblocks 2b/4.

## Verification

- Every change: `cargo fmt --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace`, `swift build` in `agent-macos`, JS
  syntax checks. Keep provider/MCP tests network-free (mock endpoints/tools).
- **Phase 2b:** select text → summarize → Send to… → Paste into frontmost app
  drops the text in.
- **Review #1:** Go Deeper on a ~1,000-line file selection does not error on the
  local model (chunks like the summary).
- **curl→ureq:** provider calls still stream; `ps` no longer shows API keys.
- **Phase 3:** run `mice mcp-server`; drive `initialize`/`tools/list`/`tools/call`
  over stdio (or an MCP inspector / Codex) and confirm `summarize_file` returns
  a local-model summary.
- **Phase 4:** configure a mock web-search MCP server; Fetch links / Go Deeper
  calls it and renders clickable links.
- **M8–M10:** per `plan/mice_planv3_files_smartcopy_agents.md`.
