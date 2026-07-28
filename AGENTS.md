# MICE contributor guidance

## Delivery order

1. Keep M0 native capability probes independent and passing before wiring M1.
2. Keep the portable Rust core separate from the macOS Swift agent (see README.md for architecture).
3. Do not persist credentials, captures, clipboard contents, model weights, or
   user configuration in this repository (Strict Boundaries in README.md).

## Architecture boundaries

- For basic architecture boundaries, see [README.md](README.md).
- Add or change protocol types in `crates/mice-ipc`; do not duplicate wire types
  in the CLI or macOS agent.
- Global input defaults to pass-through. An event may be consumed only once a
  configured gesture has been confirmed.
- Rust never renders native overlays. Swift never chooses providers or routing.

## Verification

- See [README.md](README.md) for build, lint, and test commands, which must be run after changes.
- M0 requires manual macOS permission verification for Screen Recording,
  Accessibility, and Input Monitoring. Each probe must fail clearly when its
  permission is missing.

## Multi-agent handoff protocol

Two agents work this repository — Claude Code and Antigravity — taking turns on
the same branch. This section is the shared contract; it is written to be read by
whichever agent is on shift.

**At the start of every session:**

1. Run `git log --oneline -10` to see recent work.
2. Read `.agents-sync/handoff.md` if it is non-empty — it holds notes from the
   last agent's session.

**Before ending a session, if the task is not fully resolved:** overwrite
`.agents-sync/handoff.md` with the template below. Be specific. The next agent has
no memory of your session beyond what you write there. Overwrite rather than
append — a stale prior handoff is worse than none.

### Template for `.agents-sync/handoff.md`

```markdown
# Handoff — [timestamp]
**From:** Claude Code | Antigravity
**Status:** stuck / in-progress / resolved
**Branch:** fix/bug-123

## Goal
One sentence: what bug/task are we solving.

## What I tried
- Attempt 1: [what], result: [what happened]
- Attempt 2: [what], result: [what happened]

## Current state of the code
- Files touched: `worker.py`, `test_worker.py`
- What currently works / what's still broken
- Any half-finished changes left in place (be explicit — don't let the
  next agent assume clean state)

## My hypothesis
What I think is actually going on, even if unproven.

## Specific ask for the next agent
Not "please help" — a targeted question.
e.g. "Can you check if the lock in `acquire_worker()` is released before
the retry loop re-enters? I couldn't repro locally but the stack trace
suggests a race there."

## Do NOT
Anything you tried that made things worse, or dead ends already ruled out —
save the next agent from repeating your mistakes.
```

## Local development

- See [README.md](README.md) for configuration paths, API keys, and model defaults.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
