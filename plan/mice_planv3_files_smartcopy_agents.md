# MICE Plan v3 — File-scale summarization, smart copy, and local file agents (M7–M10)

> Extends `mice_planv1.md` and `mice_planv2_selection_and_goals.md`.
> Planning document only: authored while M6c/M6d implementation is in flight on
> this branch. Implementation of M7 should start only after that work lands
> (M7 touches `mice-cli/src/main.rs` and `mice-providers`, which the in-flight
> work may also edit).

## Context

Four user needs drive this plan:

1. **Cmd+A whole-file code summarization** — select 1,000+ lines, Control
   double-tap, get "what does this file do." Today this *silently breaks*:
   the summarize path sends the raw selection to `ollama run` (subprocess,
   default 4,096-token context), so large selections truncate without warning
   and the summary covers only the top of the file.
2. **Smart copy, done right** — format-preserving table/rich-text copy via
   post-Cmd-C clipboard enrichment (the drag-interception approach is dead;
   see decisions.md).
3. **`mice tidy`** — a privacy-first local-LLM folder organizer (maps files,
   flags unused, proposes cleanup).
4. **`mice file`** — smart filing: hand MICE a file, it proposes the right
   project folder.

Product decisions already made with the user:

- Oversized inputs auto-escalate to the cloud **with a visible notice**, but
  budgets are **per-local-model**: a machine running `gpt-oss:20b` has a large
  local budget and should not escalate where a `gemma3:4b` machine would.
  Local-only mode never escalates; it chunks.
- Smart copy triggers via an **explicit gesture after a normal Cmd-C** (no
  automatic pasteboard observer in v1).
- Build order: **M7 → M8 → M9 → M10**.
- Tidy is **propose-then-confirm**; deletes only ever go to Trash and only
  individually confirmed; every applied run writes an undo log.

## Verified current-state facts (code exploration, 2026-07-16)

- `stream_ollama` (`crates/mice-cli/src/main.rs:1758`) shells out to
  `ollama run` with the prompt on stdin — no `num_ctx`, no HTTP API; the
  ANSI-stripper exists only because of the TTY-style subprocess output.
- The summarize selection path passes `selection.text` unbounded
  (main.rs:1090–1135); only the image path applies
  `bounded_for_model(…, 6000)`.
- No token counting, chunking, or code-vs-prose detection exists anywhere.
- The Swift `ClipboardSnapshot` (agent-macos/…/main.swift:60–83) already
  captures/restores **all** pasteboard flavors, and the M5 Cmd-C fallback
  already reads `.string` + `.html` (main.swift:359–374) — directly reusable
  for smart copy.
- `clipboard.set` already writes text/HTML/RTF/PNG (main.swift:543–559);
  `markdown_table_html` in `mice-core` already converts Markdown tables to
  semantic HTML.
- Config already has per-feature trigger fields and an 8-row settings TUI;
  `GoalSession` is the pattern for portable state machines in `mice-core`.

---

## M7 — Summarization at file scale (fixes the Cmd+A use case)

### 7.1 Replace the `ollama run` subprocess with the local HTTP API

Call `http://127.0.0.1:11434/api/chat` (streaming NDJSON) with
`options: { num_ctx: <model budget> }`. This is the only way to control the
context window per request. Side benefits: deletes the `AnsiStripper` hack,
removes E2BIG concerns entirely, and starts backlog item 2 (Rust HTTP client;
recommend `ureq` — small, blocking, fits the current sync architecture; the
same client later replaces the curl/argv-key calls for OpenAI/Groq).

### 7.2 Model-aware token budgets

- Add to `ModelDescriptor` (mice-providers): `input_budget_tokens` and
  `num_ctx` for local models. Seed values (validate against real 16 GB RAM
  behavior at implementation): `gemma3:4b` → num_ctx 16,384 / budget ~12k;
  `phi4-mini` → 8,192 / ~6k; `gpt-oss:20b` → 32,768 / ~24k.
- Token estimator in `mice-core`: `estimate_tokens(text)` ≈ chars/3.5 for
  code-like text, chars/4 otherwise. No tokenizer dependency — budgets carry
  headroom.

### 7.3 Routing rule for large inputs

In the selection handler / router:

```
if estimate <= local_budget(configured_local_model): local, single-shot
else if privacy mode allows cloud: escalate to the configured cloud model
     + overlay notice "Large selection — routed to <cloud model>"
else (local_only): chunked map-reduce locally (never truncate silently)
```

The per-model budget implements the no-escalate rule for big local models: a
`gpt-oss:20b` budget covers most files, so that machine simply never escalates.

### 7.4 Chunked map-reduce (local fallback path)

New `mice-core` module (portable, unit-testable, placed like `GoalSession`):
split on structural boundaries (blank lines / top-level
`fn|def|class|func|impl` patterns) into ~2.5k-token chunks → summarize each
with a bounded compact-summary prompt (sequential — parallel Ollama calls
would thrash 16 GB) → final reduce pass over the chunk summaries. Stream
overlay progress: "Summarizing part 3/8…".

### 7.5 Code-aware summarize preset

Cheap heuristic in `mice-core` (no LLM call): fraction of lines matching code
signals (braces/semicolons/keywords/indent runs). If code, use a code-summary
directive ("State what this file/module does, its main components and entry
points, and notable dependencies — a newcomer's orientation, not
line-by-line") instead of the generic prose preset. `Action::Code` already
exists as the preset slot.

### Files touched

`crates/mice-providers/src/lib.rs` (descriptors, budgets, Ollama HTTP
client), `crates/mice-cli/src/main.rs` (`stream_ollama` replacement,
selection-handler escalation + progress), `crates/mice-core/src/lib.rs`
(estimator, chunker, code heuristic, presets). No Swift or IPC changes.

---

## M8 — Smart copy v1 (explicit gesture, enrich after native Cmd-C)

### Flow

1. User copies normally (Cmd-C) — the source app writes its real
   text/HTML/RTF/image representations. Selection and dragging are never
   touched.
2. User presses the smart-copy gesture (new config field
   `smart_copy_trigger`, default `ctrl+alt+c`, new settings row — follows the
   existing M5 trigger pattern).
3. Agent reads the pasteboard (`.string`, `.html`, `.rtf`, `.png`/`.tiff`) —
   reuse the `ClipboardSnapshot` reading approach — and sends a new typed IPC
   notification
   `clipboard.captured { sessionId, text?, html?, rtfBase64?, pngBase64? }`
   (declared in `mice-ipc`, per the no-duplicated-wire-types rule).
4. Core normalizes, **deterministic first, LLM only as fallback**:
   - HTML contains `<table>` → parse and rebuild clean representations:
     semantic HTML table + **TSV as the plain-text representation**
     (spreadsheets paste TSV as a real grid — this alone fixes most "table
     format changes drastically" cases, instantly and without a model) +
     Markdown table. Reuse `markdown_table_html`; add a small HTML-table
     extractor (dependency: `tl` or `scraper`, pick the lighter at
     implementation).
   - Tabular-looking but no `<table>` (div grids, aligned text) → **local**
     LLM converts to a Markdown table → existing pipeline produces HTML/TSV.
   - Non-table rich text → local LLM cleans to Markdown → standard
     representations.
5. Write back through the existing `clipboard.set` **only on success**; any
   failure leaves the pasteboard exactly as the user's Cmd-C made it.

### Privacy rule

Clipboard content is sensitive: smart copy uses the **local lane always**,
regardless of `cloud_allowed`. A future opt-in setting may allow cloud; not
in v1. The deterministic table path uses no model at all.

### Files touched

`mice-ipc` (`clipboard.captured`), Swift agent (gesture + pasteboard read +
notification), `mice-core` (table extraction/normalization + TSV), `mice-cli`
(handler + local-lane call), settings TUI row, one small HTML-parse
dependency.

---

## M9 — `mice tidy <folder>` (local-only folder organizer)

Pure Rust-core CLI/TUI feature (portable; no Swift changes). Three passes:

1. **Metadata scan (no LLM):** recursive walk (bounded depth/file count;
   never follows symlinks out of the root; skips hidden/system directories).
   Per file: size, type, created/modified, **last-used via Spotlight**
   (`mdls -name kMDItemLastUsedDate`, subprocess, isolated in a macOS
   module), duplicates via size-then-hash. This alone yields the headline
   report: "214 files; 61 unopened >6 months (3.2 GB); 9 duplicate sets".
2. **Local LLM labeling (bounded):** for text-like files read only the first
   ~2 KB; others labeled from name+metadata. Sequential calls to the
   configured local model; hard cap on files-per-run. **Hard rule enforced in
   code, not config: file contents never go to a cloud provider, whatever the
   privacy mode.**
3. **Propose → confirm → apply:** ratatui review screen (reuse the settings
   TUI patterns) listing per-file suggested action: keep / move to
   `<category folder>` / trash-candidate. Dry-run is the default; `--apply`
   is required. Moves happen after per-run confirmation; **deletes only ever
   go to Trash and only when individually confirmed**. Every applied run
   writes an undo manifest to
   `~/Library/Application Support/MICE/tidy-log.json` (user machine, not the
   repo) enabling `mice tidy --undo`.

Default target `~/Downloads`; any folder accepted. New dependencies:
`walkdir` plus a hashing crate (e.g. `blake3`).

---

## M10 — `mice file <path>` (smart filing)

Builds on M9's scanning and undo machinery.

1. **Destination index:** `mice file --add-root ~/github` registers project
   roots. MICE indexes candidate folders (name + a one-line local-LLM
   description from README/file listing), cached in Application Support.
2. **Filing:** `mice file ~/Downloads/report.pdf` → extract features (name,
   type, small snippet if text) → local LLM ranks the top-3 destinations from
   the index → user picks/confirms → move, recorded in the shared undo log.
3. **Later (not v1):** a gesture that reads the current Finder selection
   (AX/osascript) so filing works without the terminal.

---

## Sequencing

M7 → M8 → M9 → M10, starting after the in-flight M6c/M6d work lands. Each
milestone gets a manifest entry and a decisions.md record per repo
convention.

## Verification

- Per repo rules after each milestone: `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `swift build` in `agent-macos`. Provider tests
  stay network-free (mock the Ollama HTTP endpoint).
- **M7 e2e:** Cmd+A a ~1,000-line source file, Control double-tap. Verify:
  (a) with cloud allowed — the escalation notice appears and the summary
  covers the *end* of the file too (the truncation tell); (b) in local_only —
  chunk progress messages appear and the summary is whole-file; (c) a small
  selection still routes local single-shot. Unit tests: estimator, chunker
  boundaries, escalation matrix including the gpt-oss:20b no-escalate case.
- **M8 e2e:** copy a styled table from a Chrome page, press the smart-copy
  gesture, paste into Numbers/Google Sheets (expect a real grid via TSV) and
  into Notes (expect a clean table via HTML). Verify a failed run leaves the
  original clipboard intact. Unit tests: HTML table extraction → TSV/MD/HTML.
- **M9 e2e:** run on a disposable folder seeded with old/duplicate files;
  verify the dry-run report, confirm-gated moves, Trash-only deletes, and
  that `--undo` restores everything.
- **M10 e2e:** register two project roots, file a PDF and a code file, verify
  the top-3 proposals are sane and undo works.
