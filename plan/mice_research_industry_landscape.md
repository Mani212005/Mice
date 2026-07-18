# Research: industry landscape vs. the MICE manager plan (2026-07-18)

Scope: fact-finding only. Question asked: *what do people use today to cut
frontier-agent tokens/time, and is it better than what plan v7 proposes?*
Sources linked inline; hard numbers marked. Where a fact contradicts our plan,
it is stated plainly.

## 1. The problem, stated precisely

Frontier coding agents spend most of their tokens on **operational turns**
(searching, running tools, reading output, re-stating context), not reasoning.
Measured: a small local triage layer in front of a cloud model saves **45–79%
of cloud tokens** on edit/explanation-heavy coding workloads
([Local-Splitter, arXiv 2604.12301](https://arxiv.org/pdf/2604.12301));
practitioner reports put **80–90% of agentic turns** in the "cheap lane"
([survey](https://www.digitalapplied.com/blog/small-language-models-on-device-agents-2026-guide)).
NVIDIA's position paper: SLMs are **10–30× cheaper** to serve; a heterogeneous
small/large system kept performance while cutting latency **31.6%** and API
cost **41.8%** ([NVIDIA, arXiv 2506.02153](https://arxiv.org/pdf/2506.02153),
[Arize summary](https://arize.com/blog/nvidias-small-language-models-are-the-future-of-agentic-ai-paper/)).
So the problem is real, quantified, and actively worked on. We are not early —
we are entering a crowded space with one unusual angle (see §5).

## 2. What the industry actually uses today

**a) Cheap-cloud subagent delegation — the de-facto standard in coding
agents.** Claude Code ships built-in delegation: the `Explore` subagent runs
read-only on **Haiku by default** for search/lookups; the orchestrator pattern
(Opus reasons, Sonnet implements, Haiku explores) is documented practice with
reported **5–10× cost cuts** ([Claude Code docs](https://code.claude.com/docs/en/sub-agents),
[MindStudio](https://www.mindstudio.ai/blog/smart-orchestrator-cheaper-sub-agent-models-claude-code),
[guide](https://ai.plainenglish.io/claude-code-as-an-orchestrator-using-cheaper-models-to-ship-real-projects-9c33c7f99bec)).
Haiku is ~15× cheaper per token than Opus. **This is our closest competitor,
and it requires zero user setup.**

**b) Model routing / cascades — mature, productized.** RouteLLM: **85% of
queries to cheaper models at 95% frontier quality**; FrugalGPT: up to 98%
savings on benchmarks; enterprises report **40–70%** in production; tooling is
production-ready (RouteLLM, LiteLLM, vLLM Semantic Router)
([survey](https://arxiv.org/html/2603.04445v2),
[production guide](https://tianpan.co/blog/2025-10-19-llm-routing-production),
[FrugalGPT/Portkey](https://portkey.ai/blog/implementing-frugalgpt-smarter-llm-usage-for-lower-costs/)).

**c) Local-hybrid shims — the direct precedent for plan v7.**
[Local-Splitter](https://arxiv.org/pdf/2604.12301) is almost exactly our M15:
a shim speaking MCP + OpenAI-compatible HTTP, local models via Ollama, seven
tactics (local routing, prompt compression, semantic caching, local drafting
w/ cloud review, minimal-diff edits, intent extraction, batching). Its
reliability finding matters: local models **succeeded at classification and
structured extraction, struggled at multi-step reasoning**, and it gives
"minimal detail on executing git/browser/search tools locally" — i.e. the
tool-execution-manager part of our plan is *not* what that line of work built.

**d) Parallel-agent orchestrators over git worktrees — crowded UI layer,
empty coordination layer.** Conductor (Mac app, parallel Claude Code
sessions), Claude Squad, Vibe Kanban, Composio AO (agents own their PR
lifecycle) ([Tembo roundup](https://www.tembo.io/blog/ai-agent-orchestration-tools),
[Augment list](https://www.augmentcode.com/tools/open-source-agent-orchestrators)).
Direct quote from the roundup literature: these tools "**still leave task
alignment, conflict resolution, and merge decisions on the user's plate**."
That is precisely the gap our shared-memory/`team_status` design targets.

**e) Agent memory products — cross-*tool*, not cross-*worker*.** Mem0 /
OpenMemory MCP (local-first memory server usable from Claude Code, Cursor,
etc.), Zep (temporal knowledge graph), Letta/MemGPT (OS-style memory
hierarchy) ([Mem0](https://github.com/mem0ai/mem0),
[comparison](https://www.developersdigest.tech/blog/best-ai-agent-memory-providers-2026)).
These store *facts and conversation memory the agent chooses to save*. None
auto-capture the **activity** of parallel coding agents (branches, files
touched, delegated-task outcomes) or flag overlap. Closest overlap with us:
OpenMemory MCP's local-first, MCP-served, shared-across-tools shape.

**f) Agent-OS research.** AIOS (COLM 2025): kernel with scheduler, context
manager, memory/storage managers, access control; **up to 2.1× faster** agent
serving ([arXiv 2403.16971](https://arxiv.org/abs/2403.16971)). Research-grade,
not a desktop product; validates the framing, doesn't compete on macOS.

**g) The tool-interface layer — AXI's measured claims.** AXI (
[axi.md](https://axi.md/)) = 10 principles for agent-ergonomic CLIs. Their
benchmark: AXI-style CLIs hit **100% task success at $0.074/task, 4.5 turns**,
while **MCP conditions averaged 2.3× higher input tokens** at comparable
success (their numbers: ~$185K vs ~$79K input tokens per task-set). Ecosystem
now includes **gh-axi** (GitHub), **chrome-devtools-axi**, **quota-axi**
(reports Claude/Cursor/Copilot quota windows *for routing*), sqlite-axi,
slack-axi, aws-axi, kubernetes-axi. Two consequences for us: (1) plan v7's
CLI-first registry matches the measured-best interface style; (2) **gh-axi
should replace plain `gh`** as the default GitHub adapter (evaluate first),
and **quota-axi enables quota-aware routing** — route to local when the
user's paid windows are near limits.

## 3. The uncomfortable facts (where our plan is weaker than the standard)

**Local small models are bad at driving multi-turn tool loops.** BFCL
leaderboard, scraped live from
[gorilla.cs.berkeley.edu](https://gorilla.cs.berkeley.edu/leaderboard.html)
(2026-07-18), overall agentic scores:

| Model | Overall |
|---|---|
| Claude Opus 4.5 (FC) | 77.5 |
| **Claude Haiku 4.5 (FC)** | **68.7** |
| GPT-5-mini (FC) | 55.5 |
| Qwen3-32B (FC) | 48.7 |
| Qwen3-8B (FC) | 42.6 |
| Qwen3-4B-Instruct (FC) | 35.7 |
| Gemma-3-12b-it (Prompt) | 30.4 |
| Phi-4 (Prompt) | 28.8 |
| **Gemma-3-4b-it (Prompt)** | **19.6** |

- Our default local model (gemma3:4b) scores **19.6 vs Haiku's 68.7** on
  agentic tool use. `gpt-oss:20b` is absent from the table (unverified).
- Multi-turn is the specific weakness: Qwen3-4B drops to ~35% multi-turn
  ([BFCL data](https://pricepertoken.com/leaderboards/benchmark/bfcl-v3));
  Local-Splitter found the same (fine at structured extraction, fails
  multi-step).
- Therefore: **the industry-standard delegate (a cheap frontier model like
  Haiku) is dramatically more capable at exactly the "drive a tool loop" job
  than the local SLM in our plan.** On capability-per-dollar-per-minute,
  Haiku-class delegation beats a local 4B for loop-driving. This is a fact,
  not a style choice.
- What the local lane wins on, factually: **$0 marginal cost, no plan-quota
  consumption** (Claude Code subagents still bill tokens/quota; quota-axi
  exists precisely because these windows bind), **privacy** (nothing leaves
  the machine), **offline operation**.
- Also: many delegated tasks need **no model at all**. `git status`, `gh pr
  list`, a grep — deterministic CLI + truncation. AXI's benchmark says the
  interface, not the model, was the bottleneck. The cheapest token is the one
  never spent.

**MCP `instructions` (plan 15d) is only partially load-bearing.** Claude Code
does load server instructions at session start (truncated at **2KB**)
([docs](https://code.claude.com/docs/en/mcp)); Claude Desktop stores the field
but **never reads it**
([issue #43749](https://github.com/anthropics/claude-code/issues/43749));
Codex CLI support unverified. The `mice advertise` → AGENTS.md snippet is the
reliable mechanism; `instructions` is a bonus.

## 4. Direct answer: is the industry standard better than what we're building?

Split by function, per the evidence:

- **Delegating agentic loops:** yes — today the standard (cheap cloud
  subagents, Haiku-class) is *better* than a local-4B loop-driver, by a wide
  measured margin (68.7 vs 19.6 BFCL). Our plan as written over-assigns
  loop-driving to the SLM.
- **One-shot execution + distillation** (run a deterministic tool, summarize
  output locally): the industry has *no* better standard — this is exactly
  what Local-Splitter measured as reliable for small models, and it's free,
  private, and quota-neutral. Our plan is on solid ground here.
- **Cross-agent shared memory with automatic activity capture and conflict
  early-warning:** no incumbent does this. Orchestrators punt coordination to
  the user; memory products store facts, not parallel-worker activity. This
  is the genuinely differentiated part of plan v7/M15.
- **Tool interface:** our CLI-first registry matches the measured
  state-of-the-art (AXI); adopting gh-axi/quota-axi strengthens it.

## 5. Plan adjustments implied by the facts (for plan v7 revision)

1. **Invert M13's model burden: deterministic-first, SLM-as-distiller.** The
   registry's primary mode = run the CLI deterministically, use the SLM to
   *summarize/filter output* (proven-reliable single-shot use), not to decide
   long loops. Multi-turn SLM autonomy (M14 browser loop) becomes explicitly
   experimental, with tight max-actions and escalation.
2. **Add a four-tier reasoning-budget ladder**, matching cascade literature:
   deterministic tool → local SLM (distill/extract) → cheap cloud
   (Haiku-class / GPT-5-mini via existing Groq/OpenAI lanes) → frontier.
   The cheap-cloud tier is what the plan currently lacks; it is the
   industry-standard middle and covers the SLM's loop-driving weakness.
3. **Make quota-awareness a feature:** integrate/emulate quota-axi — when the
   user's Claude/Cursor windows are near limits, bias routing local. No
   incumbent orchestrator does quota-aware routing today.
4. **Adopt gh-axi (evaluate vs `gh`) for the github.* adapter**; keep
   chrome-devtools-axi for browser.*.
5. **Bet the differentiation on M15c (shared memory + `team_status` overlap
   flagging) and the free/private/quota-neutral execution lane** — not on SLM
   loop intelligence. Ship 15c earlier if possible.
6. **Measure like Local-Splitter:** log tokens-per-delegated-task and report
   savings (per task: frontier tokens avoided, wall-clock, local model used).
   Claims need numbers; the numbers are the product's pitch.
7. **Model lane fact-check at build time:** benchmark `gpt-oss:20b` tool
   calling locally before defaulting to it (absent from BFCL); treat
   gemma3:4b as distiller-only, never loop-driver.

## Source index

- [NVIDIA: Small Language Models are the Future of Agentic AI (arXiv 2506.02153)](https://arxiv.org/pdf/2506.02153) · [Arize summary](https://arize.com/blog/nvidias-small-language-models-are-the-future-of-agentic-ai-paper/)
- [Local-Splitter measurement study (arXiv 2604.12301)](https://arxiv.org/pdf/2604.12301)
- [Dynamic routing & cascades survey (arXiv 2603.04445)](https://arxiv.org/html/2603.04445v2) · [RouteLLM/FrugalGPT production numbers](https://tianpan.co/blog/2025-10-19-llm-routing-production) · [FrugalGPT (Portkey)](https://portkey.ai/blog/implementing-frugalgpt-smarter-llm-usage-for-lower-costs/)
- [Claude Code subagents docs](https://code.claude.com/docs/en/sub-agents) · [orchestrator pattern](https://www.mindstudio.ai/blog/smart-orchestrator-cheaper-sub-agent-models-claude-code) · [cost reports](https://ai.plainenglish.io/claude-code-as-an-orchestrator-using-cheaper-models-to-ship-real-projects-9c33c7f99bec)
- [BFCL leaderboard (scraped live)](https://gorilla.cs.berkeley.edu/leaderboard.html) · [BFCL v3 mirror](https://pricepertoken.com/leaderboards/benchmark/bfcl-v3)
- [AXI principles + benchmark](https://axi.md/)
- [Orchestrator roundups: Tembo](https://www.tembo.io/blog/ai-agent-orchestration-tools) · [Augment](https://www.augmentcode.com/tools/open-source-agent-orchestrators) · [awesome-agent-orchestrators](https://github.com/andyrewlee/awesome-agent-orchestrators)
- [Mem0/OpenMemory](https://github.com/mem0ai/mem0) · [memory provider comparison](https://www.developersdigest.tech/blog/best-ai-agent-memory-providers-2026)
- [AIOS: LLM Agent Operating System (arXiv 2403.16971)](https://arxiv.org/abs/2403.16971)
- [Claude Code MCP docs (instructions, 2KB truncation)](https://code.claude.com/docs/en/mcp) · [Claude Desktop instructions gap (issue #43749)](https://github.com/anthropics/claude-code/issues/43749)
