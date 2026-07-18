# MICE plan v8 — ten multipliers: beating the industry on delegation cost + the shared-memory architecture (planner view, 2026-07-18)

Companion to `mice_planv7_slm_tool_manager.md` (M13–M16). That plan says *what*
to build; this one says what makes it **exponentially better than the
industry standard** — grounded in the research recorded in
`mice_research_industry_landscape.md`. The industry benchmark to beat:
cheap-cloud subagent delegation (Haiku-class, ~15× cheaper than Opus, 5–10×
reported savings) and routing cascades (40–85% savings). Our targets go past
that by attacking costs those approaches cannot touch.

## The core economics (facts the design is built on)

- Frontier pricing (2026-06): Opus 4.8 $5/$25 per MTok, Sonnet 5 $3/$15,
  Haiku 4.5 $1/$5. Cache **reads ~0.1×** input price; cache **writes 1.25×**
  (5-min TTL). Batches −50%.
- Therefore the savings ladder, best to worst:
  **never spend the token** (deterministic tool, cache hit) → **local token**
  ($0 marginal, no quota) → **cached frontier token** (0.1×) → **Haiku token**
  (0.2× of Opus) → fresh Opus token (1×).
- Industry routers optimize the *bottom* of that ladder (which model). MICE's
  edge is the *top*: route to **no model at all**, and make every result
  reusable across **all** connected agents. Savings that scale with the
  number of agents are the "exponential" part — a single-agent cache only
  ever self-hits; a shared manager's cache hits for every agent after the
  first.

---

## The ten multipliers

### 1. Zero-token delegation: deterministic-first execution

Most delegated operations (`git status`, `gh-axi pr list`, a grep, a file
move) need **no model**. The registry's default path is: run the CLI, apply
deterministic post-processing (truncate, count, tabulate), return. SLM only
when distillation is genuinely needed; any model call is the exception, not
the rule. AXI's own benchmark showed the interface — not the model — was the
bottleneck (100% success at $0.074/task). Implementation: every `ToolSpec`
gets a `distill: Never | IfLarge | Always` policy; `IfLarge` triggers the SLM
only when raw output exceeds the return budget (#4).

### 2. Cross-agent result cache: execute once, answer many

Because all managers share one daemon (v7 M15a), tool results are cached
**machine-wide**, keyed by `(tool, args, state-fingerprint)` — for repo tools
the fingerprint is `HEAD` + dirty-file set; for GitHub tools a short TTL; for
file summaries the file's content hash. When Codex-2 delegates a question
Codex-1 already caused MICE to answer ("summarize this module", "what PRs are
open"), the answer returns from cache: **zero tokens, milliseconds**, and the
hit rate grows with every agent added. This is Local-Splitter's semantic-
caching tactic, but placed where no incumbent puts it — *between* agents.
Store cached results in the shared memory store (`artifacts/` scope, #7) so
they double as memory. Invalidation is fingerprint-based, never time-only,
for repo-state tools.

### 3. Workflow memoization: cache the procedure, not just the result

When a delegated task completes via an SLM tool loop, record the successful
tool sequence as a **macro** (goal-pattern → parameterized script of registry
calls). Next occurrence of the same task shape replays the macro
deterministically — the model that figured out the procedure is never paid
again. Start conservative: memoize only loops that completed with all-
deterministic tools and ≤N steps; store as data (JSON list of tool calls with
arg templates), never as shell strings; macros are per-repo and verified on
replay (any tool error falls back to the live loop). This is the compiler
move — trace once, compile, reuse — and mirrors why Anthropic's programmatic
tool calling wins: intermediate results shouldn't transit a model's context.

### 4. The distillation contract: hard token budgets on every result

Every result returned upward is bounded: **≤ ~300 tokens of distilled answer
+ a `full_output_ref`** (path in the artifact store) the orchestrator can
request more of. The frontier model pays for conclusions, never logs. Two
consequences the industry pattern (Haiku subagent dumps its findings into the
parent context) doesn't get: the parent's context grows slower → its own
prompt-cache writes stay cheaper (writes are 1.25×, and every byte appended
is re-read every turn thereafter — a 5K-token tool result costs its price
**once per subsequent turn**, not once); and results become uniform enough
to cache (#2). Enforce in `ToolOutput` post-processing: `estimate_tokens` +
head/tail truncation + `truncated: true` marker (v7 M13 already specifies
the mechanism; this makes the budget a *contract*, not a safeguard).

### 5. Cache-aligned protocol design

Design MICE's MCP surface to keep the *orchestrator's* prompt cache warm:
stable, byte-identical tool list and descriptions across a session (never
regenerate descriptions dynamically — cache is a prefix match and tools
render at position 0); terse schemas; results formatted deterministically
(sorted keys, no timestamps in the body — timestamps go in a trailing field).
The `mice advertise` snippet likewise stable. This costs nothing to do and
protects the 0.1× cache-read rate the orchestrator gets on its own context —
delegation that churns the parent's prefix silently costs more than it saves.

### 6. Quota- and cost-aware routing ladder

The four-lane ladder (deterministic → local SLM → cheap cloud → frontier)
from the research doc, plus one input no incumbent uses: **quota state**.
Integrate `quota-axi` (axi.md — reports Claude/Cursor/Copilot usage windows)
as a registry tool and a routing signal: as the user's paid windows fill,
routing biases harder toward the local/deterministic lanes and `team_status`
surfaces "window at 80% — delegating aggressively to local". Escalation
between lanes is uncertainty-based (repeat failures, low-confidence parse),
per the cascade literature. Config: `routing.ladder` in `config.toml` with
per-lane enable flags so the weak-machine profile (#9) can drop lanes.

### 7. The shared-memory architecture: one store, scoped subspaces

The user's requirement: one big memory, sub-classified per agent, identical
context visible to every MICE instance. Research base: Zep's temporal
knowledge graph ([arXiv 2501.13956](https://arxiv.org/abs/2501.13956) —
three-tier episode/entity/community subgraphs, **bi-temporal** stamps),
Letta/MemGPT's RAM/disk hierarchy, A-MEM's episodic→consolidated pipeline.
We take the *shape* of that research without the database: files, daemon-
serialized, greppable.

```
~/Library/Application Support/MICE/memory/
├── events/                      # tier 1 — episodes (append-only JSONL)
│   ├── agent-<session>.jsonl    #   per-agent subspace: everything that agent
│   │                            #   did/delegated, auto-captured
│   └── shared.jsonl             #   explicit memory_note + daemon events
├── facts/                       # tier 2 — extracted state (SLM-maintained)
│   ├── agents.json              #   who: agent, branch, worktree, last-active
│   ├── touched.json             #   file → [agent, branch, last-event-id]
│   └── decisions.md             #   durable decisions ("renamed X→Y"), with
│                                #   [[links]] and provenance event ids
├── digests/                     # tier 3 — compacted narrative (SLM)
│   ├── agent-<session>.md       #   rolling per-agent digest
│   └── team.md                  #   the cross-agent picture; what team_status
│                                #   and memory_query read first
└── artifacts/                   # cached tool results + full outputs (#2, #4)
    └── <fingerprint>.json       #   {tool, args, fingerprint, distilled, raw_ref}
```

Design rules, each doing real work:

- **Every event is bi-temporal** — `{event_ts, recorded_ts}` (Zep's core
  trick): "what did agent-2 know when it branched?" is answerable, and
  out-of-order capture (a delegation that finished late) doesn't corrupt
  ordering.
- **Subspace writes, global reads.** An agent's MICE session writes only to
  its own `events/agent-*.jsonl` + `shared.jsonl`; every session reads
  everything. This is the user's "same context for MICE 1 and MICE 2" with
  provenance preserved — you always know *which* agent a fact came from.
- **Tiers are derived, never authoritative.** `facts/` and `digests/` are
  SLM-compacted *from* events on a debounce (after activity bursts, not per
  event — compaction is the distiller lane's job, gemma3-friendly). Corrupt
  or stale derived tiers are rebuildable from events; events are never
  rewritten.
- **Retrieval is deterministic-first, like everything else:** `memory_query`
  = grep/keyword over `facts/` + `digests/` + recent events → SLM composes
  the answer from the hits only. Embedding search (Ollama
  `nomic-embed-text`) is a later, optional upgrade — recency + keyword +
  structured facts covers the coordination queries that matter
  ("who touched X", "what did we decide about Y", "what is agent-2 doing").
- **Scoping levels** (global/team/private from the research notes) map to
  read policies over the same store, not separate stores: v1 ships
  `private` (agent subspace) + `shared`; `team` (repo-scoped filtering)
  lands when multi-repo use appears.
- **Size control:** per-subspace event caps with oldest-events archived
  after digestion; `digests/` bounded by the distillation contract (#4).

### 8. Conflict early-warning as a first-class feature

Built directly on `facts/touched.json`: `team_status` flags file-set overlap
between active branches (v7 M15c), then two upgrades — **line-range overlap**
(diff each branch against merge-base; overlapping hunks = red flag, same
file different regions = yellow) and **decision conflicts** (`memory_query`
over `decisions.md` when an agent's activity contradicts a recorded
decision). Delivered proactively: the daemon pushes a warning into the
*next* MCP response to the affected agents ("agent-1 has uncommitted changes
overlapping src/lib.rs:120-180"), not only when asked. The research doc's
finding stands: every orchestrator on the market leaves this to the user.

### 9. Hardware-tier capability profiles

Constraint from the user: their laptop cannot run `gpt-oss:20b` (crashes) —
gemma3:4b is the ceiling there — but stronger machines can. So the model
ladder is **detected, not assumed**:

- `mice doctor` probes: total RAM, Apple Silicon GPU cores, installed Ollama
  models → writes `machine_profile: light | standard | heavy` to config
  (overridable).
- **light** (≤16GB): gemma3:4b only — distiller lane + JSON-constrained
  single decisions; no SLM multi-turn loops (routing skips straight from
  deterministic to cheap-cloud for loop-driving). This is honest about the
  BFCL data (gemma3-4b: 19.6 agentic).
- **standard**: + phi4-mini for native tool-calls on short loops.
- **heavy** (≥32GB): + gpt-oss:20b as loop-driver — *after* the local
  tool-calling benchmark below passes.
- `mice bench-tools`: a bundled, network-free micro-benchmark (canned
  tool-call tasks, JSON-parse scoring) that validates any installed model's
  tool-calling before the router trusts it — the fact-check the research doc
  demanded for gpt-oss:20b, shipped as a command so every machine validates
  its own ladder.
- **Feedback loop** (the capability-profiles idea from the research notes):
  the savings ledger (#10) records per-task outcome per lane; a lane whose
  failure rate exceeds a threshold for a task shape gets demoted for that
  shape. Routing learns from measured outcomes, not priors.

### 10. The savings ledger: measurement as the product

Every delegation appends to `memory/events/` a ledger record:
`{task, lane, wall_ms, raw_output_tokens_est, returned_tokens_est,
frontier_tokens_avoided_est, outcome}` — where `frontier_tokens_avoided` =
tokens the orchestrator would have ingested (raw output + loop intermediate
turns) minus what it actually received. `mice savings` renders the report:
totals, per-lane breakdown, cache-hit rate (#2), macro-replay count (#3).
This is Local-Splitter's methodology productized, it feeds #9's feedback
loop, and it is the pitch: "MICE saved you N frontier tokens and M minutes
this week" is a sentence no orchestrator can currently print.

---

## gh-axi evaluation (done, 2026-07-18)

Verified locally via `npx -y gh-axi --help`: 14 commands — `issue, pr, run,
workflow, release, repo, label, project, search, secret, variable, api,
setup`, with `-R/--repo` and `--hostname` targeting and a built-in updater.
Agent-ergonomic output per the AXI principles, sits on the user's existing
`gh` auth (no keys on argv). **Decision: gh-axi is the default `github.*`
adapter in M13**, replacing the plan's plain-`gh` default; plain `gh` remains
the fallback when gh-axi is unavailable (`mice doctor` reports which is
active). `secret`/`variable` are excluded from the exposed tool set in v1
(credential surfaces; no reason the SLM needs them).

## Sequencing impact on v7

- #1, #4, #5, and gh-axi land **inside M13** (they are how the registry is
  built, not extras).
- #2 and #7's store land with **M15c**, which this plan promotes to build
  right after M13 — the shared memory + cross-agent cache is the
  differentiator; the browser rebuild (M14) can follow it.
- #6 and #9 land as the routing layer between M13 and M15 (quota-axi +
  `mice bench-tools` + machine profiles).
- #3 (memoization) and #8's line-range/decision upgrades are fast-follows
  after M15c proves the store.
- #10 starts as a logging format in M13 (cheap) and grows the `mice savings`
  report with M15.

## Verification additions

- Cross-agent cache: two mcp-server sessions, same `summarize_file` — second
  returns from cache with `cache_hit: true`, zero model calls (assert via
  mock runner).
- Memory tiers: canned event stream → `facts/touched.json` correct; digest
  regeneration is idempotent; bi-temporal query returns pre-branch state.
- Ladder honesty on light profile: a loop-shaped task on a light machine
  never routes to the SLM loop lane (unit test on router).
- `mice bench-tools` scores gemma3:4b below and (on capable hardware)
  gpt-oss:20b above the loop-driver threshold, gating the ladder.
- Ledger: a scripted session produces a `mice savings` report whose
  tokens-avoided matches hand-computed values.

## Sources

Industry numbers and per-claim sources: `plan/mice_research_industry_landscape.md`.
Additional for this doc: [Zep temporal KG (arXiv 2501.13956)](https://arxiv.org/abs/2501.13956) ·
[Graphiti](https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/) ·
Letta/MemGPT OS-style memory hierarchy · A-MEM/H-Mem consolidation papers ·
Anthropic pricing/caching docs (cache reads ~0.1×, writes 1.25×, Batches −50%) ·
[axi.md](https://axi.md/) (gh-axi, quota-axi; verified locally) ·
[BFCL](https://gorilla.cs.berkeley.edu/leaderboard.html) (scraped 2026-07-18).
