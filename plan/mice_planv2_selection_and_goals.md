# MICE Plan v2 addendum — Selection actions & Goal Guide

> Extends `mice_planv1.md`. Three user-requested features, staged as M5 and M6.
> Smart clipboard (LLM-enriched, format-preserving copy) stays deferred; its
> correct design — a post-Cmd-C pasteboard observer, never drag interception —
> is recorded in `decisions.md` and backlog item 5.

---

## 0. The three features

| # | Trigger (default) | Behavior |
|---|---|---|
| F1 | Select text, then **Control double-tap** | Summarize the current selection; stream into overlay + clipboard. |
| F2 | Select text, then **Control+Option+I** | Generate an infographic (`gpt-image-2`) from the selection; show in overlay, PNG on clipboard. |
| F3 | **Control+Option+Space** | "What is your goal today?" popup → agent builds a step-by-step plan with screen context and guides the user step by step (highlighting targets, never acting for them). |

### Hotkey reality check (macOS)

- **Cmd+Space = Spotlight**, **Cmd+Option+Space = Finder search**. An event tap
  *can* swallow these before the system acts, but hijacking Spotlight is hostile
  to users and fragile. Decision: all three triggers are **configurable in
  `mice settings`**; defaults avoid system shortcuts. The user can rebind F2 to
  `cmd+space` after freeing it in System Settings → Keyboard → Shortcuts.
- Control double-tap (two Control flag-downs within ~350 ms with no other key
  between) is chosen for F1 because plain Control is too common as a chord
  modifier, and Control+hover is already the hover-explain gesture. Guard
  against misfires: the gesture only fires when a non-empty selection exists.

---

## 1. Shared foundation: selection acquisition (no drag interception)

Both F1 and F2 need "the text the user selected" **without touching mouse
events**. Selection stays 100% native; MICE reads it only after a keyboard
gesture.

Order of attempts (Swift agent, new `SelectionReader`):

1. **AX selected text.** Focused element's `kAXSelectedTextAttribute`; if
   absent, walk to the focused web area (Chrome/Safari expose selected text on
   the AXWebArea). Fast, silent, no clipboard side effects.
2. **Synthesized Cmd-C fallback** (PopClip-style) when AX yields nothing:
   snapshot the current pasteboard change-count + contents, post Cmd-C to the
   frontmost app, wait ≤150 ms for the change-count to bump, read text/HTML,
   then **restore the previous pasteboard**. Only runs on explicit gesture, so
   it is user-initiated per our privacy rules; contents are never persisted.
3. If both fail → overlay message "Select some text first."

New IPC (extend `mice-ipc`, per AGENTS.md rule — no wire types elsewhere):

- Agent → core notification `selection.text` `{ sessionId, text, html?, source: "ax"|"clipboard", action: "summarize"|"image" }`.
- Reuse existing `overlay.show/appendResult/finishResult`, `clipboard.set`.

Rust side: `selection.text` events route exactly like today's OCR path —
`Action::Summarize` or `Action::Image` with `artifacts.text` — so the existing
router, presets, providers, and clipboard engine are reused unchanged.

### Acceptance (M5)

- Select a paragraph in Chrome, double-tap Control → summary streams in the
  overlay and lands on the clipboard; the page's blue selection is never
  disturbed; pasteboard is unchanged unless the fallback ran (then restored).
- Select a table, press Control+Option+I → infographic renders in the image
  panel and pastes as PNG.
- Gesture with no selection → clear overlay hint, no model call.
- `cargo test` covers gesture parsing/config; Swift builds; manual check in
  Chrome, Notes, and a PDF viewer (AX-poor case exercises the fallback).

### Build steps (M5)

1. `mice-ipc`: add `selection.text` params type + tests.
2. Swift agent: `SelectionReader` (AX first, Cmd-C fallback w/ restore);
   double-tap-Control detector and second configurable action trigger in the
   existing tap (handle `.tapDisabledByTimeout` path already fixed).
3. `mice settings`: two new rows — "Summarize selection trigger",
   "Infographic trigger" (validated list + free-form chord string).
4. CLI `start()` loop: handle `selection.text`, dispatch by `action` field.
5. Docs: manifest capabilities + decisions entry.

---

## 2. M6 — Goal Guide ("What is your goal today?")

The confirmation-gated task-planning feature from `decisions.md`, now with a
concrete UX. MICE **plans and points; the user acts.** No autonomous clicks,
form submissions, credentials, or purchases — ever (existing decision, kept).

### 2.1 Flow

```
Control+Option+Space
  → agent shows native input panel: "What is your goal today?"
  → user types goal, Enter
  → agent sends goal + context snapshot to core
  → core asks cloud model (strict JSON schema) for a step plan
  → PLAN REVIEW: overlay lists steps; user accepts / asks to revise (loops)
  → GUIDE MODE: one step at a time in a floating panel
       [ Step 2 of 6 ] "Open chrome and go to smallpdf.com"   (Next ▸  Back ◂  Quit ✕)
  → for browser steps: reuse browser-ext bridge to highlight the target
    for native-app steps: AX lookup → overlay.highlight box on screen
  → user does the action, presses Next (v1: manual advance)
  → done → summary overlay ("Goal complete 🎉")
```

### 2.2 Context the planner sees

Per step (and at planning time): frontmost app name, window title, current
URL when the browser bridge is connected, OCR of a screen capture (existing
path), and the bounded AX summary. Captures feed the model only; nothing is
persisted (repo rule).

### 2.3 New pieces

| Piece | Where | Notes |
|---|---|---|
| `overlay.promptInput {title, placeholder}` command + `prompt.submitted {sessionId, text}` notification | `mice-ipc` + Swift panel with NSTextField | Was already sketched in plan v1 §9.5. |
| `GoalSession` state machine (`Idle → Planning → Reviewing → Guiding(step) → Done/Aborted`) | new `mice-core` module | Portable, unit-testable, no I/O. |
| `goal_plan_payload()` strict-schema request: `{ steps: [{instruction, app_hint, browser_target?: string, done_check?: string}] }` | `mice-providers` | Same pattern as guide payload; provider per privacy mode (cloud lane; LocalOnly uses local model with a notice about quality). |
| Step panel (Next/Back/Quit buttons or Ctrl+N/Ctrl+B keys) | Swift overlay | Buttons need a small interactive panel — first interactive overlay; keep keyboard shortcuts as fallback. |
| Step highlighting | reuse `overlay.highlight` (native, AX-located bounds) and browser bridge candidate flow (browser) | Bridge gains a "highlight for current guide step" request initiated by core rather than the popup. |
| Persona/tone setting ("patient teacher" default, selectable) | config + prompt template | Cheap: one line in the system prompt; serves the elderly-user use case. |

### 2.4 Staging

- **M6a** — popup + plan generation + plan-review loop in the overlay (text
  only). Acceptance: type a goal, get a numbered plan, revise it once.
- **M6b** — guide mode with manual Next/Back and native AX highlights.
- **M6c** — browser-step highlighting through the existing bridge; core, not
  the popup, drives the bridge.
- **M6d (later)** — auto-advance: model checks `done_check` against a fresh
  context snapshot when the user presses "Check me". Full auto-detection
  (screen diffing) deferred.

### 2.5 Safety rails (restating the standing decision)

- Every step is advisory; MICE highlights and instructs but posts no clicks or
  keystrokes during guide mode.
- Steps that involve logins, payments, personal data: the plan schema marks
  them `sensitive: true`; the panel shows "Do this yourself, then press Next"
  and MICE never requests or displays credentials.
- Goal text and snapshots follow existing rules: runtime only, never persisted.

---

## 3. Order of work

1. **M5** (selection summarize + infographic) — small, reuses nearly everything;
   also forces the pasteboard-restore machinery that the future smart-copy
   observer will need.
2. **M6a → M6c** as above.
3. Backlog items already recorded (vision payloads, curl→Rust HTTP, `mice stop`,
   backpressure) continue in parallel; the vision payload item directly
   improves M6 context quality and should land before M6b.
