# MICE Plan v5 — M12: Web Autopilot & Companion (the observe→decide→act loop)

> Extends plan v4. Supersedes M11d (absorbed into M12b). Planning document
> only; M11c is in flight and M12 builds directly on M11a/b/c machinery.

## Context — the product this is actually for

The mission, in the user's words: **an AI helper for people who are weak with
technology.** They write (eventually say) a goal — "log in to this website and
do X", "open my Google Sheet and tell me the total" — and MICE either does it
for them or patiently walks them through it, on the real screen, in plain
language.

Two test scenarios exposed why the current Goal Guide cannot deliver this:

1. *"Search Canva on Google, click the Canva result, open a portrait."*
2. *"Open my Google Sheet and tell me the sum of column D."*

### Why it fails today (architectural, not model, failure)

The Goal Guide **plans once, then executes a static step list** (M6a). It
writes "click the Canva result" before it has ever seen the results page.
Real pages surprise: popups, consent banners, login walls, layout variance.
Every working web agent (Operator, browser-use, Claude-in-Chrome) is instead
an **observe → decide → act loop**: after *every* action the model receives a
fresh observation of the page plus its own action history and any error, and
chooses exactly one next action. Static plans navigate blind; loops navigate
by sight.

### Feasibility verdict (honest)

- **Scenario 1 class (multi-step navigation): YES** with a frontier cloud
  model (`gpt-5.6-sol` default; `llama-3.3-70b-versatile` acceptable) driving
  the loop over the existing verified-candidate DOM machinery. Expect 70–90%
  unassisted success on tasks of this size, not 100% — the design below makes
  the failure mode "ask the human for one click," not "give up."
- **Local models never drive autopilot.** `gemma3:4b`/`phi4-mini` are not
  reliable enough for consequential multi-step decisions; autopilot is
  cloud-lane by definition and refuses to start in `local_only` mode with a
  clear message. Local stays for private text work (summaries, hover).
- **Scenario 2 splits in two.** Navigating *to* the sheet: the loop handles
  it (including finding it via Drive search when no link is given). *Reading*
  the sheet: Google Sheets renders cells on a **canvas** — DOM snapshots see
  almost nothing. Reading needs the **vision observation path** (screenshot →
  cloud vision model), i.e. backlog item #1, which becomes a real dependency
  here (M12c). With vision, "sum the visible column D" works; very long
  columns need scrolling passes and remain best-effort.

### Decisions locked with the user

- **Consent once per goal** in autopilot: at start MICE says what it will do
  ("I'll click and type for you and narrate each step; I always stop and hand
  over for logins and payments — start?"). One yes, then it runs. **Esc stops
  instantly at any moment.** Per-action confirmation (plan v4's choice)
  survives as an optional "careful mode" setting and remains the default for
  the *first* run so users see how it behaves.
- **Hard stops are unchanged and non-negotiable in every mode:** passwords,
  one-time codes, payment fields, and final-submission/purchase actions are
  never performed by MICE — it highlights, explains, and waits (M11c
  enforcement, doubled in Rust and the content script).

---

## The loop (core design, shared by both modes)

One portable state machine in `mice-core` (pattern: `GoalSession`), driven by
the CLI, acting through the M11a native-messaging port.

```
AgentLoop {
  goal: String,
  mode: Autopilot | Guide,
  history: Vec<CompactTurn>,   // action taken + one-line result, bounded
  budget: IterationBudget,     // default 15 actions, config-capped
}

each turn:
  OBSERVE   fresh bounded observation (see below)
  DECIDE    one cloud call, strict JSON schema (below)
  ACT       Autopilot: execute via browser.act (M11b, verified candidates)
            Guide:     highlight target + speak instruction; wait for the
                       page to change (M12b) or the user to ask for help
  RECORD    append compact turn; loop until done / stuck / stopped / budget
```

**Observation** (bounded, built from existing pieces): URL + page title +
ranked candidate list (existing `rank_guide_candidates` snapshot) + the last
action's outcome/error + optional screenshot (M12c). History is compacted to
one line per past turn so context stays small and cheap.

**Decision schema** (strict structured output, same discipline as the guide
schema — the model picks candidate IDs, never selectors):

```json
{
  "say_to_user":  "Now I'm opening the Canva website…",   // plain language, always present
  "action":       "click | fill | openUrl | scroll | done | handoff | ask_user",
  "candidate_id": "candidate-3",          // for click/fill
  "url":          "…",                    // for openUrl
  "value":        "…",                    // for fill; never credentials
  "done_summary": "…",                    // for done: what was accomplished / the answer
  "question":     "…"                     // for ask_user
}
```

`handoff` is the graceful-degradation action: the model highlights the target
and asks the user to do that one thing (sensitive targets are *forced* to
handoff by the safety layer regardless of what the model says). `ask_user`
pauses for a clarification ("Which of your sheets — 'Budget' or 'Budget
2026'?"). `done` ends with a spoken summary — which is also how scenario 2
returns "the sum is 4,120."

**The degradation ladder is the product:** Autopilot → stuck → single-step
handoff ("I've highlighted it — please click there") → resume autopilot. The
tool never dead-ends; it becomes a teacher exactly when it stops being a
chauffeur.

---

## Milestones

### M12a — Agentic loop core (autopilot over M11 hands)

- `AgentLoop` state machine + turn compaction in `mice-core` (unit-testable,
  no I/O — decision transitions tested against canned model outputs).
- `agent_loop_payload()` in `mice-providers`: strict schema above; system
  prompt carries the safety rules, the narration requirement, and the
  iteration discipline (one action per turn, prefer handoff over guessing).
- CLI: goal popup gains an **Autopilot** choice next to the reviewed-plan
  guide (goal-level consent screen shown here); loop executor wires
  OBSERVE (bridge snapshot) → DECIDE (cloud call) → ACT (`browser.act`).
- Overlay narrates every `say_to_user`; terminal echoes the audit line per
  action (M11c behavior). Esc (existing panel Quit + a global key check)
  aborts between turns and mid-stream.
- Budget/stop conditions: max actions, max consecutive failures (2 on the
  same target → forced handoff), wall-clock cap.

### M12b — Live page observation & the follow-along guide (absorbs M11d)

- Content script installs a `MutationObserver` + SPA navigation hooks
  (`history.pushState`/`popstate`) and pushes debounced
  `browser.pageChanged { url, title }` events over the persistent port.
- **Autopilot** uses it to know an action settled before observing again (no
  blind sleeps).
- **Guide mode transforms:** highlight a step, and when the page changes
  appropriately the loop *observes and advances by itself* — the elderly user
  never presses Next; MICE follows along like a teacher watching over their
  shoulder, re-planning from what's actually on screen. (This replaces
  M6b's manual Next/Back as the primary flow; manual stays as fallback.)
- "Where?" and "Check me" from old M11d fold in naturally: Where? = fresh
  observe + re-highlight; Check me = one no-action DECIDE turn.

### M12c — Vision observation (unblocks canvas apps & the Sheets scenario)

- Prerequisite: backlog #1 — carry captured PNGs into provider payloads
  (`input_image` parts in `openai_responses_payload`; Groq path is text-only,
  so vision turns route to the OpenAI lane).
- Loop gains screenshot-on-demand: when the DOM snapshot is sparse (few/no
  candidates — the canvas tell) or the model requests it, attach a bounded
  screenshot of the focused window/region (existing ScreenCaptureKit path) to
  the next DECIDE turn.
- Acceptance is scenario 2: "open my Google Sheet and tell me the sum of
  column D" — navigate via DOM, read via vision, answer via `done_summary`.
  Document the limit: data must be brought on-screen (scrolling passes are
  best-effort v2).

### M12d — Companion UX (the tech-weak-user layer)

- **Narration-first overlay:** larger type option, one calm sentence per
  action, no jargon, progress feel ("Step 3 — opening your sheet…").
- **Personas** (config, one system-prompt line): patient (default) / concise /
  playful — the "fun teacher / strict teacher" the mission calls for.
- **Safety theater made visible:** at every hard stop, a distinct panel state:
  "This is a password box — I never type these. Please type it, then I'll
  continue." (Trust is built at exactly these moments.)
- **First-run careful mode:** the first autopilot goal runs with per-action
  confirm so the user watches it work before granting goal-level consent.
- Later (explicitly out of M12): voice in/out via `gpt-realtime-mini`
  (plan v1 §5 already reserves it), native-app autopilot via AXPress.

---

## What still won't work (set expectations)

- Sites with hard bot defenses, CAPTCHAs → always handoff.
- Long multi-page workflows (20+ actions) → budget-split into sub-goals;
  v1 asks the user to re-invoke.
- Reading large datasets from canvas apps → visible-region only in M12c.
- 100% reliability → not a thing; the ladder (autopilot → handoff → guide) is
  the honest answer, and for this audience it is genuinely the better UX.

## Sequencing

M11c (in flight) → **M12a → M12b → M12c → M12d** → then plan v3's M7–M10
(M7 summarization scale-up remains next after M12; its Ollama-HTTP work is
independent and could interleave if a second agent is free). Manifest +
decisions.md entries per milestone.

## Files touched

`mice-core` (AgentLoop, turn compaction, budgets), `mice-providers`
(agent-loop payload/schema, vision parts in M12c), `mice-cli` (loop executor,
autopilot consent flow, Esc abort, sparse-snapshot screenshot trigger),
`mice-ipc` (pageChanged + any new panel states), `browser-ext/content.js`
(MutationObserver/navigation events), Swift agent (narration overlay states,
consent + hard-stop panels, first-run careful mode).

## Verification

- Standard gates (fmt/clippy/test network-free with canned model turns,
  swift build, JS checks).
- **M12a e2e (scenario 1):** goal "search Canva on Google, open Canva, open a
  portrait" with consent-once. Expect: narrated search → result click →
  site open → portrait click, ≤10 actions, no confirmation prompts after
  consent, Esc abort verified mid-run. Also verify `local_only` refuses
  autopilot with the clear message.
- **M12b e2e:** same goal in Guide mode — user performs the clicks, steps
  advance without pressing Next. Popup/banner mid-flow: loop observes it and
  routes around (or hands off) instead of clicking a stale target.
- **M12c e2e (scenario 2):** "open my Google Sheet and tell me the sum of
  column D" — navigation via DOM, read via screenshot turn, spoken answer.
- **M12d checks:** hard-stop panel appears on a test login form with the
  "I never type these" message; personas change tone; first-run careful mode
  confirms each action exactly once.
- **Loop robustness unit tests:** forced-handoff after repeated failure,
  budget exhaustion, ask_user pauses, sensitive-target override of a model
  `click` decision.
