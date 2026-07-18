# MICE plan v7 — the SLM execution manager: CLI tools as agent tools (planner/reviewer view, 2026-07-18)

## Vision

MICE grows from a desktop copilot into a **local execution manager**: a small
language model (SLM) layer that sits between expensive frontier agents and the
machine, executing operational work (browser, git/GitHub, files, search) so
frontier models spend their reasoning budget on reasoning.

```
Human
  │
  ├─ Codex #1 (branch A, worktree A) ──┐     ← reason, write code, emit intent
  ├─ Codex #2 (branch B, worktree B) ──┤
  │        (each spawns its own thin   │ MCP (stdio, per agent)
  │         `mice mcp-server`)         ▼
  │                     MICE daemon (`mice start`)   ← ONE manager brain
  │                       ├── local SLM (Ollama, already built)
  │                       ├── tool registry (NEW: browser./github./git.*)
  │                       └── shared memory  ← sees ALL agents' activity
```

The frontier agents do not share context with each other — but every MICE
instance is a window onto the **same daemon and the same memory**, so the
manager layer has the cross-agent picture the agents individually lack
("Architecture B: per-agent managers", with a shared brain).

Two faces, both grounded in what already exists:

- **Upward (server, Phase 3 done):** `mice mcp-server` exposes cheap local
  abilities. This plan extends it so an orchestrator can *delegate execution*,
  not just summarization.
- **Downward (executor, NEW):** the SLM drives external CLI tools through a
  uniform tool registry, using the M12 observe→decide→act machinery that
  already exists in `AgentLoop` (`crates/mice-core/src/lib.rs:292`).

### Key architectural insight: CLI-first, not MCP-client-first

The tools the user found (chrome-devtools-axi, and the axi family at
https://axi.md/) are **already agent-ergonomic CLI wrappers over MCP servers**.
`chrome-devtools-axi` wraps Google's `chrome-devtools-mcp` behind a persistent
bridge and exposes 35 single-shot commands (`open`, `snapshot`, `click @uid`,
`fill @uid`, `fillform`, `eval`, `network`, `screenshot`, …) with plain-text
output designed to be grepped. Verified locally via `npx -y chrome-devtools-axi
--help` (2026-07-18).

So MICE does **not** need an MCP client implementation to consume these tools.
It needs a **tool registry that shells out to CLIs and post-processes output
for the SLM**. That is dramatically simpler, fits the dependency-light
philosophy, and reuses the subprocess patterns already in `mice-cli`. A true
MCP client (plan v6 Phase 4) stays on the roadmap for tools that exist *only*
as MCP servers — it becomes M16, not the entry point.

The same registry tools are then **re-exported upward** through
`mice mcp-server`, which is exactly the user's phrasing: "make these CLI tools
become MCP tools for our agent."

### Verified current state (2026-07-18)

- `mice mcp-server` (stdio, hand-rolled JSON-RPC, protocol 2025-06-18) is live:
  `mcp_server()` at `crates/mice-cli/src/main.rs:3477`, tools
  `summarize_text/summarize_file/explain_code/define_word/quick_answer`
  (`mcp_tools()` :3561), all routed to the local model with M7 chunked
  map-reduce (`mcp_summarize` :3650).
- M12 autopilot loop exists and works mechanically: bounded observation
  (`MAX_OBSERVATION_CHARS`, :111), turn-based loop, `AgentDecision {
  action: Click|Fill|OpenUrl|Scroll|Done|Handoff|AskUser }`
  (`mice-core/src/lib.rs:253`), `AgentLoop` state machine with budgets
  (:292), careful-mode. Its weakness was never the loop — it was the custom
  extension chain (native host ↔ MV3 worker ↔ content script; see
  `plan/mice_m12_review.md` H0–H17) and text-only cloud decisions.
- Ollama HTTP `/api/chat` with `num_ctx` is in (`stream_ollama_chat`,
  `mice-providers`); `ureq` is already a dependency.
- MODELS table (`mice-providers`) includes `gpt-oss:20b` (24k budget) and
  `phi4-mini` — both support native tool/function calling in Ollama.
  `gemma3:4b` does **not** (needs JSON-constrained prompting, which M12's
  snake_case `AgentDecision` JSON already proved out).
- Still open from plan v6: curl→ureq migration (9 sites, API-key-on-argv),
  Phase 5 (M8–M10).

### chrome-devtools-axi facts that shape this design (from `--help` + skill)

- Persistent bridge auto-starts on first command; session survives across
  invocations; `stop` tears down. Bridge port 9224 by default.
- `CHROME_DEVTOOLS_AXI_SESSION=<name>` gives each session its own bridge,
  port, and state → **multiple isolated browser sessions in parallel**. This
  maps 1:1 onto "Architecture B: per-agent managers" from the research notes.
- `CHROME_DEVTOOLS_AXI_AUTO_CONNECT=1` attaches to the user's real running
  Chrome (144+, remote debugging enabled) instead of launching a fresh one —
  the M12 "act in the user's browser" experience without our extension.
- Snapshots carry `uid=` refs with a generation prefix (`g<N>:`); acting on a
  re-rendered page fails loudly with `STALE_REF` → solves M12's stale-element
  problem (H11/H15) at the tool layer.
- `screenshot <path>` gives the vision-escalation input M12 lacked.
- Cold start: `npx -y` bootstrap can take ~30s; mitigation is
  `npm install -g chrome-devtools-mcp` + `CHROME_DEVTOOLS_AXI_MCP_PATH`.

> Note: axi.md itself could not be re-fetched while writing this plan (network
> tooling outage on the planning session). Before implementation, verify: the
> exact axi tool family list, whether an axi GitHub CLI exists (else default to
> `gh`), and chrome-devtools-axi's current version/flags via
> `npx -y chrome-devtools-axi --help` and `update --check`.

---

## M13 — Tool registry + SLM tool-calling loop (the core of the manager)

New module `tools.rs` in `mice-cli` (or a `mice-tools` crate if it grows):

### Registry

```rust
struct ToolSpec {
    name: &'static str,          // "browser.snapshot", "github.pr_checks"
    description: &'static str,   // one line, written for a small model
    args: &'static [ArgSpec],    // name, type, required, description
    kind: ToolKind,              // ReadOnly | Mutating
    run: fn(&ToolCall, &ToolContext) -> Result<ToolOutput, ToolError>,
}
```

- `ToolContext` carries the session name (→ `CHROME_DEVTOOLS_AXI_SESSION`),
  working directory, timeouts, and output budget.
- `ToolOutput` is post-processed before the SLM sees it: truncate to a token
  budget (reuse `estimate_tokens`, mirror the `MAX_OBSERVATION_CHARS`
  approach), keep head+tail on overflow, and record `truncated: true` so the
  model knows.

### Built-in adapters (v1)

- **browser.\*** → `npx -y chrome-devtools-axi <cmd>`:
  `open <url>`, `snapshot`, `click <uid>`, `fill <uid> <text>`,
  `fillform`, `press`, `scroll`, `back`, `wait`, `eval`, `screenshot`,
  `pages/newpage/selectpage`, `stop`. Pass uids through verbatim (including
  the `g<N>:` prefix). Session name = goal session id.
- **github.\*** → `gh` (already authenticated via `gh auth`, keys never on
  argv): `repo_view`, `pr_list`, `pr_view`, `pr_checks`, `pr_diff`,
  `issue_list`, `issue_view`, `run_list`, `run_view` (read-only set first;
  `pr_create`, `issue_comment` are Mutating tier).
- **git.\*** → `git`: `status`, `log`, `diff`, `branch` (read-only);
  `add`/`commit`/`push` are Mutating tier and land only after the read-only
  set is proven.

### Safety tiers

- `ReadOnly` tools run freely inside a session.
- `Mutating` tools respect the existing `careful_mode`
  (`AutopilotConfig`, `mice-core`): overlay confirm (reuse `PromptInput` /
  guide-step UI) before executing. Browser `click`/`fill` count as Mutating
  when `careful_mode` is on, ReadOnly otherwise (same policy M12 used).
- Never pass secrets on argv; subprocess env is inherited minus
  provider API keys (allowlist PATH/HOME/CHROME_DEVTOOLS_AXI_*).

### SLM tool-calling

Two lanes, chosen by `model_descriptor`:

1. **Native tool calls** (preferred): Ollama `/api/chat` with a `tools` array
   built from the registry — works with `gpt-oss:20b` and `phi4-mini`.
   Extend `mice-providers` with `ollama_chat_tools_payload(model, messages,
   tools)` and parse `message.tool_calls`.
2. **JSON-constrained fallback** (gemma3 and other non-tool models): reuse the
   M12 decision pattern — system prompt lists the tools and demands a single
   snake_case JSON object `{"tool": "...", "args": {...}, "say_to_user":
   "..."}`; parse with serde exactly like `AgentDecision`. M12 already proved
   small/fast models can hold this contract.

Generalize the loop: today `AgentAction` is browser-shaped
(`Click|Fill|OpenUrl|Scroll|…`). Introduce `ToolDecision { tool: String,
args: Value, say_to_user, done, ask_user }` in `mice-core`, with unit tests,
and keep `AgentLoop`'s state machine (budgets, Paused/HandedOff/Done) as the
generic driver. `AgentDecision` remains for the legacy extension path until
M14 retires it.

### CLI surface

- `mice tools` — list registry tools and per-tool availability (checks `gh`
  auth, node/npx present, chrome reachable) → doubles as `mice doctor`
  extension.
- `mice do "<goal>"` — run the SLM tool loop headless (terminal output), the
  test harness for everything above. Options: `--model`, `--max-actions`,
  `--session <name>`.

### Acceptance

- `mice tools` lists browser/github/git tools with availability.
- `mice do "what PRs are open on this repo and did CI pass on the newest?"`
  completes using only `github.*` tools + the local SLM, no cloud calls.
- `mice do "open example.com and tell me the page title"` completes via
  browser.open → browser.snapshot (or eval) → Done.
- A Mutating tool with careful_mode on shows a confirm before running.
- Unit tests: registry schema → Ollama `tools` payload; `ToolDecision`
  parsing (native + JSON fallback); output truncation; env scrubbing.
  All network-free (mock command runner).

## M14 — Browser autopilot v2 on chrome-devtools-axi (extension retired)

Rebuild the M12 experience on the registry instead of the custom extension
chain — this deletes the entire fragile leg (native host ↔ MV3 worker ↔
content script) that produced H0, H8, H10, H11, H13, H15.

- **Observe** = `browser.snapshot` (uid-refs; truncate to observation budget
  with the same collapse rules as M12's `MAX_OBSERVATION_CHARS`).
- **Act** = `browser.click/fill/fillform/press/scroll/open`.
- **Settle** = the CLI's own `wait <ms|text>` + STALE_REF retry: on
  `STALE_REF`, take a fresh snapshot and let the model re-decide (one retry,
  then escalate) — replaces H11's hand-rolled MutationObserver settle.
- **Verify** = after each Mutating action, fresh snapshot before claiming
  success (the skill's own contract; fixes the M12 "clicked but nothing
  happened" class).
- **Sessions**: default to an isolated profile; offer
  `CHROME_DEVTOOLS_AXI_AUTO_CONNECT=1` mode ("act in my Chrome") as a config
  flag `autopilot.attach_to_user_chrome` — document the Chrome 144+ /
  remote-debugging requirement in `mice doctor`.
- **Escalation (reasoning budgets)**: SLM decides each turn by default; after
  N stalled turns (no page change / repeated STALE_REF / model uncertainty),
  escalate the *same* observation to the configured cloud lane
  (Groq/OpenAI, code already exists in `call_*_agent_turn`), optionally with
  `browser.screenshot` for vision. Uncertainty-based escalation, not
  always-cloud — this inverts M12, which was cloud-always.
- `mice autopilot` gains `--engine axi|extension` (default `axi`); the
  overlay narration (`autopilot_status`/`autopilot_narrate`) is reused
  unchanged. The extension path stays until v2 is proven, then is removed
  along with `setup-browser`.
- **Performance**: document the global-install mitigation
  (`CHROME_DEVTOOLS_AXI_MCP_PATH`) in `mice doctor` / README; first-run
  `npx -y` cold start is otherwise ~30s.

### Acceptance

- The Canva scenario from `plan/mice_m12_review.md` (open canva.com, reach
  "Create a design", report next step to the user) passes with `--engine axi`
  with **no extension installed**.
- Kill Chrome mid-run → loop reports a tool error and pauses (no silent
  stall). STALE_REF path covered by a test with a mocked runner.
- SLM handles ≥ the mechanical turns locally; escalation fires only on stall
  (assert via turn log).

## M15 — The manager pairing: delegation, shared memory, capability advertisement

Target scenario: two (or more) frontier agents work the same repo on different
branches in different worktrees — Codex 1 on branch A, Codex 2 on branch B.
Each gets a MICE manager; every manager shares one brain and one memory. The
frontier model does the core thinking and writing; when it judges a step
mechanical, repetitive, or token-heavy, it hands that step to MICE with just
enough context, and keeps working. **The delegation decision belongs to the
big model** — MICE's job is to make delegating obvious, cheap, and safe.

### 15a — Topology: one daemon, many thin MCP servers

- `mice start` (already the resident daemon owning the bridge socket) also
  owns: the tool registry sessions, the shared memory store, and an **agent
  session table**.
- `mice mcp-server` becomes a **thin client** — the same daemon/thin-client
  split `mice autopilot` already uses. Each coding agent spawns its own stdio
  instance via its MCP config; the instance forwards requests over the
  daemon's local socket, tagged with its session id. Daemon not running →
  fall back to today's standalone summarize-only behavior and say so in tool
  output (degraded mode, never a hard failure).
- **Session registration** at `initialize`: record `clientInfo.name`, pid,
  cwd, and the git branch/worktree resolved from cwd. The daemon now knows
  "session 1 = Codex on branch A at ~/wt-a" without anyone telling it.

### 15b — Delegation: `delegate_task` (plus the direct tools)

- `delegate_task { instruction, context?, max_actions? }` — the frontier
  model hands down a step with context; the daemon runs the M13 SLM tool
  loop **in the calling session's worktree** and returns
  `{ outcome, summary, actions_taken }` distilled by the SLM. One tool call
  for the orchestrator; all the intermediate tokens are local.
- Direct tools for one-shot offloads (no loop needed):
  - `run_tool { name, args }` — any ReadOnly registry tool, post-processed.
  - `browser_task { goal, max_actions }` — bounded M14 loop, summary back.
  - `git_summary {}` — status+log+diff distilled into a short brief.
  - `repo_grep { pattern, path }` — bounded grep with SLM-filtered results;
    cache query→result keyed by pattern+HEAD (semantic-cache seed).
- **Mutating policy upward (v1):** git mutations only in the calling
  session's own worktree and only when the instruction explicitly asks
  (commit/push); everything else ReadOnly. careful_mode overlay confirm
  still applies. No cross-worktree writes, ever.
- Tool descriptions state token-cost intent ("cheap, local, seconds") so
  orchestrators route eagerly.

### 15c — Shared memory: one store for all managers

- Store: append-only `events.jsonl` under
  `~/Library/Application Support/MICE/memory/` plus a periodically
  SLM-compacted `digest.md`; daemon-serialized writes, no database
  dependency. Entry shape:
  `{ ts, session, agent, branch, kind, text, files? }`.
- **Auto-recorded** (this is what makes the shared brain real): every
  delegated task and its outcome, every tool-run summary, branch status
  deltas, and files touched per session.
- MCP tools on top:
  - `memory_note { text }` — an agent explicitly tells its manager something
    worth remembering ("we decided to rename X to Y").
  - `memory_query { question }` — SLM answers over digest + recent events:
    "what is the other agent working on?", "was this approach already
    tried?".
  - `team_status {}` — one distilled brief per active session: agent, branch,
    last activity, files touched — **with overlapping files flagged** (the
    merge-conflict early warning between branches).
- Scoping (global/team/private layers from the research notes) is deferred
  to M16; v1 memory is global to the machine, which matches the stated goal:
  MICE 1 and MICE 2 have the same context.

### 15d — Capability advertisement: making Codex realize it has a manager

Two mechanisms, both cheap, do this "tell your skills to Codex" step:

1. **MCP `initialize` `instructions` field** — the MCP spec lets the server
   return an `instructions` string at initialize, which clients (Codex,
   Claude Code) surface to the model. Return a compact paragraph: "You are
   paired with MICE, a local manager agent. Delegate mechanical, repetitive,
   or token-heavy steps — git/GitHub queries, browser checks, file
   summaries, repo searches — via `delegate_task` or the direct tools; they
   run locally in seconds and cost you one call. Check `team_status` before
   editing files other agents may hold; record decisions with `memory_note`."
2. **`mice advertise`** — prints (or writes with `--into <file>`) a snippet
   for AGENTS.md / CLAUDE.md / Codex instructions listing the live tool set,
   when to delegate, and the shared-memory etiquette — so the pairing
   survives clients that ignore `instructions`. Generated from the registry,
   never hand-maintained.

### Acceptance

- Two `mice mcp-server` instances (simulating Codex 1 / Codex 2 in two git
  worktrees) against one daemon: `delegate_task` from each executes in its
  own worktree; `team_status` from either lists both sessions with correct
  branches; `memory_note` written via one is answered by `memory_query` on
  the other.
- Both sessions touch the same file → `team_status` flags the overlap.
- Daemon down → mcp-server still serves the summarize tools and reports
  degraded mode; no hang, no crash.
- `initialize` returns the instructions paragraph; `mice advertise` output
  matches the live registry.
- Smoke test drives `run_tool`/`browser_task`/`git_summary`/`delegate_task`
  over line-delimited stdio with a mock runner + canned SLM (network-free).

## M16 (later) — true MCP client + research backlog

- **MCP client**: `[mcp.servers]` in `config.toml`; spawn stdio servers,
  `initialize`/`tools/list`/`tools/call` (we already speak this dialect from
  the server side); imported tools register into the same registry with a
  `mcp.<server>.<tool>` prefix. Unlocks tools that have no CLI wrapper.
- **Research backlog** (from the SLM-manager notes; record, don't build yet):
  event-driven completion monitoring (agents emit TaskCompleted → MICE
  notifies the human with an SLM summary + suggested next prompt);
  capability profiles + outcome feedback for routing; memory scoping
  (global/team/private layers over the M15 store); interruptibility;
  cost-aware scheduling as a
  first-class `route_task()` in `mice-core` (deterministic tool vs SLM vs
  cloud — `route_selection_summary` is the seed of this).

## Cross-cutting

- **curl→ureq** (from plan v6, still open): do it during M13 — the tool loop
  makes provider calls and must not leak keys on argv (review finding #2).
- **Model lanes**: keep `gemma3:4b` as the summarize/distill lane; default the
  tool-calling lane to `gpt-oss:20b` when installed, else `phi4-mini`, else
  gemma3 with the JSON fallback. Add `tool_model` to `Config` beside
  `local_model`.
- **Docs**: README + `mice doctor` cover: node/npx, `gh auth login`,
  optional `npm i -g chrome-devtools-mcp` + `CHROME_DEVTOOLS_AXI_MCP_PATH`,
  Chrome 144+ for attach mode.

## Sequencing

M13 (registry + `mice do`) → M14 (autopilot v2, retire extension) → M15
(manager pairing: 15a topology → 15b delegation → 15c shared memory → 15d
advertisement; 15a+15b can ship before 15c) → M16 (MCP client + research
items). curl→ureq lands
inside M13. Plan v6 Phase 5 (M8–M10 file features) stays queued behind M15 —
or in parallel if a second implementer is available, since it touches
different code.

## Verification (every milestone)

`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `swift build` in `agent-macos`. All new tests
network-free: tool adapters take a command-runner trait so tests inject canned
CLI output (including a canned `STALE_REF` and a canned snapshot with `g1:`
uids). Live checks (`mice do`, autopilot v2, MCP delegation) are manual
acceptance items listed per milestone above.

## Open choices (flagged, defaulted)

1. **GitHub tool — RESOLVED (2026-07-18)**: `gh-axi` (verified locally: 14
   agent-ergonomic commands over the user's existing `gh` auth) is the
   default `github.*` adapter; plain `gh` is the fallback. See
   `mice_planv8_exponential_multipliers.md` § gh-axi evaluation.
2. **Tool-calling model — RESOLVED as hardware-tiered**: the ladder is
   detected per machine (`machine_profile: light|standard|heavy` via
   `mice doctor` + validated by `mice bench-tools`), not assumed. Light
   machines (like the user's laptop, which cannot run gpt-oss:20b) use
   gemma3:4b as distiller-only with no SLM loop lane; heavy machines enable
   gpt-oss:20b as loop-driver after passing the local benchmark. See plan v8
   multiplier #9.
3. **Registry location**: start as `tools.rs` in `mice-cli`; promote to a
   `mice-tools` crate when M15 needs it from more than one binary.
4. **Attach vs isolated Chrome**: default isolated profile (safe); attach
   mode opt-in via config.
