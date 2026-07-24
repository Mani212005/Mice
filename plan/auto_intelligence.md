# MICE plan — Auto: making a fixed 4B model smarter, without changing it (2026-07-24)

> Direct sequel to `plan/auto.md` (Record → Replay → Repair, M17a–d — all four
> now implemented and live-debugged). This document picks up exactly where
> that one left off: M17a–d fixed the *mechanics* (14 distinct bugs found and
> fixed live against real Wikipedia runs — process leaks, parser bugs, hard
> crashes on malformed model output, over-broad safety gates, a truncated
> observation budget). What's left, confirmed by direct observation, is
> genuine model-capability variance, not bugs. This document is the research
> pass on what to do about *that*, under the same hard constraint as always:
> 100% local, 100% private, gemma3:4b (or ~5B), no bigger/cloud model, ever.

## 0. The ask, verbatim intent

After M17a–d shipped and got live-tested against `en.wikipedia.org` repeatedly,
the same simple goal ("go to en.wikipedia.org and search for the James Webb
Space Telescope") sometimes completed cleanly and sometimes didn't — not from
crashes or corruption (those are now fixed and verified), but from the model
itself: re-toggling a "Main menu" button three times in a row for no reason,
handing off early out of uncertainty, occasionally still producing garbled
output even with grammar-constrained decoding. The user's framing: Wikipedia
is **Level 1** of a five-level difficulty ladder they want MICE to eventually
climb, ending at **Level 5** — search across *multiple different websites*,
extract structured data (e.g. restaurant listings) from each, and write the
aggregated result into a Google Sheet. Multi-tab, multi-site, real productive
output. The explicit ask: deep-research what can make the *same* fixed local
model handle this reliably — including whether an "RL adapter" or local
fine-tuning loop is a real, practical lever — not by swapping in a smarter
model.

## 1. Research pass — self-improvement without changing the base model

*(Full fork report; condensed here with the load-bearing findings.)*

### 1.1 Non-parametric self-improvement (no weight updates at all)

**Reflexion** ([Shinn et al., 2303.11366](https://www.semanticscholar.org/paper/Reflexion:-language-agents-with-verbal-learning-Shinn-Cassano/0671fd553dd670a4e820553a974bc48040ba0819))
remains the foundational pattern — an agent verbally critiques its own failed
trajectory and stores the reflection in an episodic buffer to bias the next
attempt. A 2026 follow-up, [Process Supervision via Verbal
Critique](https://arxiv.org/pdf/2604.21611), decomposes the single
end-of-episode signal into step-indexed critiques, so a trajectory that got
the search box right but fumbled the click after isn't discarded wholesale —
directly relevant, since that's the exact shape of MICE's partial failures.

Two papers land closer to home than generic Reflexion:

- **[Memento](https://arxiv.org/abs/2508.16153)** ("Fine-tuning LLM Agents
  *without* Fine-tuning LLMs", Aug 2025) formalizes case-based memory with a
  *separately learned, lightweight* retrieval policy on top of a case bank —
  4.7–9.6 point gains on out-of-distribution tasks. This is a rigorous
  upgrade path for MICE's existing recipe system: right now retrieval is
  pure embedding-similarity (§4 Pillar C, `plan/auto.md`); Memento's
  contribution is learning *which* precedent to retrieve, without ever
  touching gemma3:4b's weights.
- **[SkillWeaver](https://arxiv.org/pdf/2504.07079)** (Apr 2025) is a
  web-agent-specific loop: the agent discovers primitive action sequences,
  practices them, and distills successful ones into reusable, *parameterized*
  APIs — not raw click logs, actual callable skills like
  `search_product(query)`. Reported 31.8–39.8% relative success-rate gains on
  WebArena and real sites, and — the number that matters most for MICE —
  **skills synthesized by a strong agent boosted a weaker agent's success
  rate by up to 54.3%** when handed to it. Direct evidence a small model
  doesn't need to be smart enough to *discover* a good interaction pattern;
  it only needs to be smart enough to *execute* one already handed to it —
  which is precisely the Record→Replay→Repair bet `plan/auto.md` already
  made, now with independent research backing it specifically.

### 1.2 Parametric self-improvement (local fine-tuning / the "RL adapter" idea)

**STaR/ReST lineage generalizes to agentic trajectories.** The pattern —
generate rollouts, keep only the ones a verifier confirms correct,
supervised-fine-tune on that self-purified set, repeat — has been extended
into tool-use settings (DART: models spontaneously learn tool invocation from
their own successful rollouts, no human annotation). This is **rejection
sampling + SFT, not RL** — no reward model, no policy-gradient math, just
"keep the good ones, fine-tune on them." That distinction is the whole
feasibility story.

**Modern RL frameworks (ART, RAGEN, Agent Lightning) all assume a CUDA GPU
training backend.** [ART](https://art.openpipe.ai/) (OpenPipe, GRPO-based,
produces LoRA checkpoints) needs vLLM server-side; there's no credible native
Apple Silicon path. [RAGEN](https://github.com/mll-lab-nu/RAGEN) and
[Agent Lightning](https://arxiv.org/abs/2508.03680) (Microsoft, wraps
existing agents with near-zero code changes) have the same requirement. This
is a hardware-assumption mismatch against the "100% local, Mac" constraint,
not a maturity problem.

**LoRA/QLoRA fine-tuning specifically for a 4B model on a Mac is real and
fast.** A comparable-size model (Mistral-7B) QLoRA-trains on 5,000 examples
in ~90 minutes on an M2 Max 32GB, ~7GB peak RAM
([source](https://towardsdatascience.com/lora-fine-tuning-on-your-apple-silicon-macbook-432c7dab614a/)).
gemma3:4b is roughly half that size. Serving the result locally via Ollama is
a documented, solved problem: convert the LoRA adapter to GGUF
(`convert_lora_to_gguf.py`), load via a `Modelfile`'s `ADAPTER` directive.

**Real risk if this is built: catastrophic forgetting.** Production
postmortems list "forgot established behavior after a capability update" as
a recurring failure mode. Mitigation (EWC regularization, replay of old
examples, or just LoRA's inherent adapter-isolation) is real engineering
surface — a held-out regression eval suite has to gate every new adapter
before it deploys, or a "smarter" nightly fine-tune could silently make the
agent worse at something it used to do fine.

**Verdict from this fork, stated plainly:** build the memory/skill-library
layer (§1.1) now — near-zero engineering and compute cost, zero regression
risk, and it's a rigorous upgrade to something already built. Treat local
LoRA-SFT (STaR/ReST-style, no reward model) as a genuinely feasible but
materially bigger second-phase bet — build it once the memory layer's
returns visibly plateau, not before, and only with a regression suite in
place first. Full RL (GRPO/ART/RAGEN) is out of scope for this project's
hardware constraint, full stop, not "maybe later."

## 2. Research pass — reliability engineering for harder, longer tasks

*(Full fork report; condensed here with the load-bearing findings.)*

### 2.1 The difficulty ladder is real and already has a name

WebArena (812 tasks, single-site): frontier agents reach 61.7%; **sub-10B
models score 21.7%** — consistent with this repo's own BFCL finding
(gemma3:4b: 19.6 vs Claude Haiku: 68.7 on agentic/multi-turn tool use,
`plan/mice_research_industry_landscape.md`). Treat ~20–25% *unaided*
single-site completion as gemma3:4b's honest baseline, not a bug. Mind2Web
shows success collapsing further under live (non-snapshot) web conditions —
even OpenAI's Operator tops out near 61% live. AndroidWorld shows success
declining monotonically across explicit easy/medium/hard tiers, with model
scaling (8B→32B) giving diminishing, not proportional, returns within a
method.

**Level 5 already has a name and a fresh benchmark**:
[Odysseys](https://arxiv.org/abs/2604.24964) (CMU, Apr 2026) was built
because "existing benchmarks have converged on short, single-site tasks...
while real use requires long-horizon, multi-site workflows" — 200 real
live-internet tasks, rubric-graded (avg. 6.1 rubrics/task) rather than binary
pass/fail, because at this horizon partial, gradeable progress is the
realistic unit of measurement, not all-or-nothing success. This is the field
independently confirming the L1→L5 framing is a real, distinct,
currently-unsaturated research axis — not something invented for this
project.

### 2.2 Why small models specifically get stuck (mechanistically) — and what fixes it

"Small models struggle to generate reliable structured outputs, get stuck in
repetitive loops, waste context with redundant text, and fail to decide when
to stop" ([Small Language Models for On-Device Agents, 2026](https://www.digitalapplied.com/blog/small-language-models-on-device-agents-2026-guide)).
Mechanistically, autoregressive models can enter self-reinforcing loops
driven by risk-aversion toward a harder-but-correct action, with the
trajectory becoming attention-locked into repetition
([LoopGuard, 2604.10044](https://arxiv.org/pdf/2604.10044)). And directly on
point: "without deterministic state validation, agents hallucinate tools,
miss exit conditions, and fall into infinite retry loops... the problem is
not a bad prompt — it is a lack of execution infrastructure" (AskUI, 2026).
That is exactly the thesis this session's 14 fixes already acted on
(pre-flight verification, capped soft-retries, corruption detection) — this
research independently confirms that was the *correct* first lever, not
premature optimization.

Techniques that map to concrete next moves:

- **Early handoff is not purely bad — lean into it.**
  ["Runaway is Ashamed, But Helpful"](https://arxiv.org/abs/2505.17616)
  (EMNLP Findings 2025) studies exactly what we watched (an agent
  handing off/aborting early) and finds a relay pattern — weak agent hands
  off, stronger agent finishes with the same total step budget — beats
  forcing the weak agent to grind on. MICE's `ExecutionLane` local→cloud
  escalation (already built, `plan/auto.md` §1) *is* this pattern; the
  finding says lean into it rather than trying to eliminate early handoff.
- **Doomed trajectories are detectable early; abort, don't retry blind.**
  ["Doomed from the Start"](https://arxiv.org/abs/2607.06503) (Jul 2026,
  tested on Llama-3.2-3B — gemma3:4b's class) shows failure is predictable
  from round 1, saving 37–47% of inference compute by aborting instead of
  continuing. MICE's `consecutive_replans` cap (this session) is a crude
  behavioral proxy for the same idea, without hidden-state access via
  Ollama — validates capping harder rather than trusting self-correction.
- **History should be structured state, not a growing transcript.**
  [AgentProg](https://arxiv.org/abs/2512.10371) (Dec 2025) reframes agent
  history as a program with variables/control-flow plus a belief-state,
  hitting SOTA on AndroidWorld's long-horizon suite specifically because raw
  growing history "incurs substantial context overhead" and "fails to
  preserve vital semantic information." Direct upgrade path from this
  session's `describe_action_for_history` (uid-free, but still free text)
  toward typed state (`visited_sites`, `extracted_so_far`, `current_subgoal`)
  the model reads structurally instead of re-parsing prose every turn.
- **Observation frequency should decouple from action frequency.**
  [Signal-Driven Observation](https://arxiv.org/pdf/2606.06708) argues
  re-rendering the full page every step causes "progressive context
  degradation" over a long session; a lightweight signal detector (URL
  change, new elements, action failure) should decide *when* a full
  re-observation is even warranted.
- **Test-time compute helps small models — but only with a cheap verifier.**
  A 1B model can rival a 70B one on checkable math via enough sampling — but
  only because the answer is cheaply verifiable. A browser click has no
  built-in equivalent *unless you build one* — which MICE already has,
  piecemeal (uid-resolution, `is_soft_execution_refusal`,
  `already_has_value`, corruption detection, all from this session). The
  actionable version: sample 2–3 candidate decisions at a step, run each
  through the existing deterministic checks *before* committing, execute the
  first (or majority) that passes all of them. Best-of-N gated by a verifier
  already built, not a new model capability.
- **Explicit milestone/subgoal tracking reduces wasted exploration.**
  [ADMIRE](https://arxiv.org/abs/2602.11524) and
  [MiRA](https://arxiv.org/html/2603.19685v1) are RL-training techniques
  (dense, dynamically-updated milestone rewards), but the no-training,
  inference-time analog is usable today: give the model an explicit, visible
  checklist for the goal ("1. navigate ✓ 2. locate search field 3. enter
  query 4. submit") and require a one-line progress self-report against it
  each turn.

### 2.3 Multi-tab / multi-site orchestration (Level 5)

2026 production frameworks converge on one shape. **Skyvern 2.0**
([June 2026](https://www.skyvern.com/blog/skyvern-changelog-june-2026/))
runs a formal Planner → Actor → Validator loop and added genuine multi-tab
support specifically because single-flow tab-jumping was dead-ending.
**[Multi-Agent Computer Use / MACU](https://arxiv.org/abs/2606.01533)**
(Salakhutdinov et al., Jun 2026) is the most transferable architecture: a
manager decomposes the objective into a DAG of sub-tasks, dispatches
*parallel* sub-agents against the ready frontier, and revises the DAG as
results stream back — +3.4–25.5% success, up to 1.5x faster than serial
execution on long-horizon tasks specifically.

The key insight for a 4B-only constraint: **MACU's manager and every
sub-agent can be the same small model**, called multiple times with narrow,
per-site context — no single call ever holds the whole multi-site history,
which is exactly what sidesteps the context-degradation problem §2.2
diagnoses. Concretely, for "search N restaurant sites, write to a sheet": one
lightweight gemma3:4b manager call decomposes the goal into a queue of
independent "search site X, extract listing" sub-tasks; each sub-task runs
as its own bounded, fresh-context autopilot invocation (which MICE's
process-per-invocation CLI design already supports naturally); each writes
its structured result to a shared **external** scratchpad (a file, or the
target sheet itself) — never back into the model's own context; the manager
does a final light aggregation/dedup pass over the scratchpad. This is the
parallel-dispatch layer missing from MICE's existing recipe system, not a
replacement for it — recipes make each individual site-visit fast on repeat;
MACU-style decomposition is what fans one goal out across many site-visits in
the first place.

### 2.4 Honest ceiling

None of the above raises gemma3:4b's actual *judgment* — deciding a listing
looks legitimate, resolving conflicting info across two sites, noticing a
result is subtly wrong is reasoning capability, and the WebArena/AndroidWorld
scaling data (8B→32B: roughly +10 points, not proportional) says architecture
buys execution *reliability*, not reasoning *ceiling*. The realistic target
with everything below applied: much higher completion rates on
well-specified, mechanically-decomposable tasks — which "list 1,000
restaurants" actually is (extraction, not judgment) — with the existing
`ExecutionLane` cloud-escalation path remaining the correct release valve for
genuinely ambiguous decisions, not something to engineer away.

## 3. If you had to pick three

In priority order, reasoned from both forks:

1. **SkillWeaver-style parameterized skills over the existing recipe
   library** (§1.1). Directly upgrades what M17c already built; near-zero
   regression risk; the single biggest evidence-backed lever ("boosted a
   weaker agent by up to 54.3%") that doesn't touch the model at all.
2. **Execution-infrastructure hardening, continued** (§2.2): explicit visible
   sub-goal checklists, Best-of-N gated by MICE's own existing deterministic
   verifiers, structured (not free-text) history. This is the same lever
   this session's 14 fixes already pulled, confirmed by research as the
   *correct* one — not prompt-tuning, more execution scaffolding.
3. **MACU-style manager/DAG decomposition for Level 5** (§2.3) — the only
   approach in the literature that lets a small model produce a genuinely
   long-horizon, multi-site aggregate result without a single call ever
   holding more than one site's worth of context.

Deliberately *not* picked first: local LoRA fine-tuning (§1.2). Real and
feasible on the user's own Mac, but a bigger lift with real regression risk,
and both forks independently converge on "build once the cheaper levers
plateau, not before."

## 4. Milestones

Continues `plan/auto.md`'s M17a–d (all four implemented and live-debugged
this session — see the 14-bug fix list in commit history from `2c5ec75`
through `9a9056c`).

### M18a — Parameterized skill library (upgrades M17c)

- Recipe format gains a `parameters` field: instead of a raw uid/click
  sequence tied to one literal goal string, extract the varying part (e.g.
  the search query) as a named slot, so `search-wikipedia(query)` matches
  *any* goal shaped like "search Wikipedia for X," not just a re-run of the
  exact same X.
- Retrieval matches on the *template* (goal shape with the slot blanked),
  not the literal goal text — directly fixes the site-key/goal-text
  mismatch limitation already flagged in `plan/auto.md` §8.
- **Acceptance:** running the same goal *shape* with a different search term
  than the one that taught the recipe (e.g. "search for black holes" after
  teaching on "James Webb Space Telescope") replays successfully with zero
  fresh decisions for the shared steps, only the fill's slot value differs.

### M18b — Structured belief-state instead of free-text history

- Replace `history: Vec<String>` (or supplement it) with typed fields the
  model reads structurally: `visited_urls`, `extracted_so_far`,
  `current_subgoal`, `attempts_on_current_subgoal`.
- Explicit, visible sub-goal checklist derived from the goal text at
  planning time (reuses `GoalSession`'s existing `Planning` phase); each
  turn's prompt includes it and requires a one-line self-report against it.
- **Acceptance:** a goal requiring 8+ steps shows measurably fewer repeated
  / redundant actions (via `describe_action_for_history` dedup) than the
  same goal run against unstructured history, logged and compared like
  `plan/mice_research_industry_landscape.md`'s existing methodology.

### M18c — Verifier-gated Best-of-N at flaky decision points

- At the specific decision points already proven unreliable live (the "what
  should the very next action be" call), sample 2–3 candidate decisions from
  gemma3:4b instead of one.
- Run each candidate through MICE's *already-built* deterministic checks
  (uid resolves via `resolve_current_uid`, passes `is_soft_execution_refusal`
  pre-checks, not flagged by the smart-quote corruption check, not a
  redundant fill via `already_has_value`) before committing; execute the
  first candidate that passes all of them, or the majority if several do.
- **Acceptance:** on the Wikipedia-search live benchmark (this session's
  repeated manual runs), completion-without-replan rate improves measurably
  over single-sample baseline across N repeated runs of the identical goal.

### M18d — MACU-style manager/DAG orchestration for multi-site goals

- A new, thin orchestration layer *above* `mice autopilot` (not a rewrite of
  it): a manager call decomposes a Level-5-shaped goal ("search N sites for
  X, write results to sheet Y") into a queue of independent, bounded
  sub-goals, one per site.
- Each sub-goal runs as its own fresh `mice autopilot` invocation (process
  isolation MICE already has for free); results are written to an external
  structured scratchpad (a local file, or the target sheet directly), never
  fed back into any single model call's context.
- Manager does a final light aggregation/dedup pass over the scratchpad.
- **Acceptance:** a scripted 3-site "collect one fact from each, write to a
  local CSV" goal completes with no single autopilot invocation's context
  ever containing more than one site's worth of history, and the aggregate
  output is correct across all 3 sites.

### Deferred, explicitly not this phase — local fine-tuning loop

- STaR/ReST-style rejection-sampling + local LoRA-SFT of gemma3:4b on
  verified-successful trajectories (from M18a's growing skill library),
  served via Ollama's `ADAPTER` Modelfile directive.
- Gated on: M18a–d's returns visibly plateauing, and a held-out regression
  eval suite existing first (to catch catastrophic forgetting before a new
  adapter ships) — both forks converge independently on this sequencing.

## 5. Sequencing

M18a (skill library) and M18b (structured state) can proceed in parallel —
neither depends on the other. M18c (Best-of-N verification) is cheapest and
can ship alongside either. M18d (multi-site orchestration) depends on M18a
(recipes need to be reliable per-site before fanning out across many sites)
and benefits from M18b (each sub-agent needs clean, bounded context to stay
inside the "no single call holds multi-site history" constraint). The
deferred fine-tuning loop depends on all four.

## Source index

- [Reflexion (Shinn et al.)](https://www.semanticscholar.org/paper/Reflexion:-language-agents-with-verbal-learning-Shinn-Cassano/0671fd553dd670a4e820553a974bc48040ba0819)
- [Process Supervision via Verbal Critique](https://arxiv.org/pdf/2604.21611)
- [Memento (2508.16153)](https://arxiv.org/abs/2508.16153)
- [SkillWeaver (2504.07079)](https://arxiv.org/pdf/2504.07079)
- [SAGE (2512.17102)](https://arxiv.org/html/2512.17102v2)
- [ART](https://art.openpipe.ai/) / [GitHub](https://github.com/OpenPipe/ART)
- [RAGEN](https://github.com/mll-lab-nu/RAGEN)
- [Agent Lightning (2508.03680)](https://arxiv.org/abs/2508.03680)
- [LoRA fine-tuning on Apple Silicon](https://towardsdatascience.com/lora-fine-tuning-on-your-apple-silicon-macbook-432c7dab614a/)
- [Fine-tuning for Ollama deployment](https://medium.com/@kapildevkhatik2/fine-tuning-for-ollama-a-practical-tutorial-prereqs-deploy-7aee429bd0c3)
- [Catastrophic forgetting in continual-learning agents](https://zylos.ai/research/2026-04-09-continual-learning-catastrophic-forgetting-ai-agents/)
- [WebArena Benchmark](https://www.emergentmind.com/topics/webarena-benchmark)
- [Online-Mind2Web Benchmark](https://www.emergentmind.com/topics/online-mind2web)
- [Odysseys: Benchmarking Web Agents on Realistic Long-Horizon Tasks (2604.24964)](https://arxiv.org/abs/2604.24964)
- [Runaway is Ashamed, But Helpful (2505.17616)](https://arxiv.org/abs/2505.17616)
- [Doomed from the Start (2607.06503)](https://arxiv.org/abs/2607.06503)
- [AgentProg (2512.10371)](https://arxiv.org/abs/2512.10371)
- [Signal-Driven Observation (2606.06708)](https://arxiv.org/pdf/2606.06708)
- [ADMIRE (2602.11524)](https://arxiv.org/pdf/2602.11524)
- [MiRA (2603.19685)](https://arxiv.org/html/2603.19685v1)
- [Multi-Agent Computer Use / MACU (2606.01533)](https://arxiv.org/abs/2606.01533)
- [Skyvern Changelog — June 2026](https://www.skyvern.com/blog/skyvern-changelog-june-2026/)
- [LoopGuard (2604.10044)](https://arxiv.org/pdf/2604.10044)
- [Orca: Browsing at Scale (2505.22831)](https://arxiv.org/pdf/2505.22831)
- [Small Language Models for On-Device Agents in 2026](https://www.digitalapplied.com/blog/small-language-models-on-device-agents-2026-guide)
- Internal: `plan/auto.md` (Record→Replay→Repair, M17a–d),
  `plan/mice_research_industry_landscape.md` (BFCL benchmark table)
