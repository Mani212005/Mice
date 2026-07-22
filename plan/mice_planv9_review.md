# MICE Plan v9 — code review (bugs)

> Reviewer notes on the uncommitted working-tree diff implementing plan v9
> (unified command palette, local `UserHistory`/memory, Keychain-backed
> provider keys), the "Coordination Mesh" module
> (`crates/mice-cli/src/coordination.rs`, see
> `.lavish/mice-coordination-mesh-plan.html`), and the newer "Mission
> Control" module (`crates/mice-cli/src/mission.rs`, see
> `.lavish/mice-mission-control-plan.html`). No source was changed as part
> of this review. References are `file:line` against the working tree.
>
> **Update (2026-07-22, second pass):** the branch moved forward
> significantly between the two review passes (Mission Control added
> entirely; the Swift agent and `coordination.rs` both grew). Nearly every
> finding from the first pass below has since been fixed by the other agent
> working on this branch — confirmed by re-reading the current code, not
> assumed. Each first-pass finding is marked **FIXED** with the current
> evidence, or **STILL PRESENT** where it isn't. New findings from the
> second pass (Mission Control validator gaps, and one real regression
> introduced by a first-pass fix) are listed in their own section below.

## Summary

The architecture and stated invariants hold up well overall. Privacy
bounding in `UserHistory` (memory.rs), the palette/menu IPC wire shapes,
the Ollama warm-path work, and — as of the second pass — the Keychain save
path, goal-plan JSON parsing, `mice ask` history handling, Escape/focus
handling in the Swift palette, and the Coordination Mesh's rename/lockfile/
timeout handling are all now correct. The two things still worth fixing are
below: a genuine remaining session-leak edge case in Goal Guide review, and
a new AXI snapshot-parsing desync bug introduced by an otherwise-correct
fix for the earlier "multi-line label" issue.

---

## OPEN — still present

### O1. GoalSession leak: Escape on the *reviewed-plan* panel never notifies core

Status: **STILL PRESENT** (re-verified against current code).

The "Start guide / Revise / Cancel" plan-review UI is rendered through the
generic result `panel` via `overlay.show`/`overlay.result`
(`crates/mice-cli/src/main.rs:5934-5941`, `goal_review_actions()` at
`main.rs:5902-5917`, Swift's `showActions` at
`agent-macos/Sources/MiceMacAgent/main.swift:2094-2105`) — it is a
different surface from both `PalettePanel` and `GuideStepPanel`.

`OverlayController.dismiss()` (`main.swift:2014-2030`), which global Escape
invokes via `dismissActive()` (`main.swift:183-185`):

```swift
func dismiss() {
    panel.orderOut(nil)                                   // <- review panel, no IPC sent
    let paletteWasVisible = palettePanel?.isVisible == true
    palettePanel?.dismissAndRestoreFocus(notifyCore: true) // only notifies for the palette
    if !paletteWasVisible {
        guidePanel?.dismissFromGlobalEscape()               // only notifies for the *active-guide* step panel
        guidePanel = nil
    }
    ...
}
```

There is no code path that sends `goal.cancel`, `prompt.cancelled`, or
`palette.dismissed` for the review-plan surface specifically. Rust's
cleanup for a `Reviewing` session only fires on the explicit `goal.cancel`
action id (`main.rs:5954-5971`), on `prompt.cancelled` (`main.rs:4298-4304`),
or on `palette.dismissed` (`main.rs:4334-4348`) — none of which Escape
triggers on this path.

**Failure scenario:** generate a plan, review it, press Escape instead of
clicking Cancel. The `goal_sessions`/`goal_plans` `HashMap` entries stay in
`GoalState::Reviewing` in the daemon indefinitely; they're only reclaimed
if the person later types a *new* palette `plan …` (which does
`sessions.retain(...)`, `main.rs:5686-5693`) or the daemon restarts. Low
severity (bounded by daemon restart / next plan, no privacy impact — a
plan/goal is not sensitive captured content), but real, and it's the same
class of bug already fixed for the initial goal-entry prompt and the
Guide-step panel; this is the one remaining surface that wasn't covered.

**Fix direction:** have `OverlayController.dismiss()` also notify core
about the currently-shown review panel (e.g. reuse `prompt.cancelled` or a
new `overlay.dismissed` keyed by the review panel's session id) when
neither the palette nor the guide panel was the dismissed surface.

---

## NEW — found in the second pass

### N1. HIGH, security-relevant — AXI snapshot line-pairing desync on an escaped newline inside a page-controlled label

`crates/mice-cli/src/tools.rs:100-155` (`BrowserSnapshot::from_axi_output`),
`:193-223` (`strip_quoted_spans`)

This is a **regression introduced by the fix** for the earlier "AXI
multi-line label confirmation context" finding. `from_axi_output` now pairs
original and structural lines positionally: `output.lines().zip(structural_output.lines())`
(`tools.rs:108`). But `strip_quoted_spans` swallows a backslash-escape
immediately followed by a literal newline without emitting that `'\n'` into
`structural_output`:

```rust
if in_quote {
    if escaped { escaped = false; continue; }   // <- also eats a following '\n'
    if character == '\\' { escaped = true; continue; }
    ...
```

So `structural_output` ends up with **one fewer line** than `output`
whenever a quoted (page-controlled) label ends a physical line with a lone
`\`. Because `.zip()` pairs by position, every original line from that
point on pairs with the *wrong* structural line, and the tail of the
snapshot silently drops out of pairing.

Verified with a standalone reproduction of the function bodies:

```
output   = button "Evil\<NL>type=submit uid=g9:fake" uid=g5:real<NL>link "Cancel" uid=g7:cancel<NL>
structural (2 lines instead of 3):
  button    uid=g5:real
  link    uid=g7:cancel
```

Effects: a legitimate target (`g7:cancel`) drops out of the snapshot
entirely (no structural partner), and `g5:real`'s uid gets attributed to
the wrong original line's context/role from that point on. This is
directly attacker-reachable — any web page can set an accessible label
ending in a literal backslash right before more label text on the next
line — and it undermines exactly the guarantee this code exists to
provide: that AXI's human-confirmation text shows the target's *true*
current label/context before a browser action is confirmed
(`decisions.md` M13/M14, "structural data is only ever read from the
unquoted remainder").

**Root cause:** `strip_quoted_spans`'s `escaped` state is scoped across the
whole document in one pass, while the per-line `quoted_context` pairing
loop resets `escaped = false` on every call — the two quote-tracking paths
disagree about whether an escape can span a line boundary, and only
`strip_quoted_spans` can consume (and thus drop) a structural newline.

**Fix direction:** `strip_quoted_spans` must always preserve exactly one
`'\n'` per physical input line regardless of escape state (or the pairing
code must otherwise guarantee
`structural_output.lines().count() == output.lines().count()` before
zipping).

### N2. Mission Control: `MissionTaskGraph::validate()` never checks "unsafe parallel scope"

`crates/mice-core/src/lib.rs:168-249`

The Mission Control plan states verbatim: *"Deterministic validation
rejects cycles, missing acceptance, or unsafe parallel scope."*
`validate()` correctly checks task-ID validity/uniqueness, title length,
empty acceptance, self/duplicate dependencies, unknown dependencies,
dependency cycles (a correct Kahn's-algorithm topological sort), and
per-task predicted-path traversal safety — but never checks whether two
tasks with **no dependency edge between them** (i.e. schedulable in
parallel) declare overlapping `predicted_paths`.

**Failure scenario:** two independent `MissionTask`s, `task-a` and
`task-b`, neither depending on the other, both with
`predicted_paths: ["src/lib.rs"]` and otherwise-valid fields — `validate()`
returns `Ok(())`. This is exactly the "two agents editing the same file
concurrently with no coordination" case the plan says must be rejected.

Currently dormant: the CLI's Markdown-derived task synthesis
(`mission.rs:322`) always sets `predicted_paths: Vec::new()`, so this can't
fire through `mice mission plan` today. But `validate()` is the module's
documented safety boundary for "later model planners"
(`lib.rs:143-145,165-167`) — once a model's structured task proposal is
wired through this same function (M1/M2 per the plan), unsafe-parallel
plans will silently validate as safe.

### N3. Mission Control: `is_safe_predicted_path` has bypasses for non-POSIX absolute-path forms

`crates/mice-core/src/lib.rs:260-267`

Only rejects a leading `/`, a backslash anywhere, and `.`/`..`/empty path
segments. Does **not** reject:
- A forward-slash Windows drive path, e.g. `"C:/Windows/System32/x"` —
  splits to non-empty, non-`.`/`..` segments, accepted as "safe."
- A `~`-prefixed path, e.g. `"~/.ssh/id_rsa"` — doesn't start with `/`, no
  backslash, accepted.
- Embedded NUL bytes.

The specifically-checked `a/../../etc/passwd` case **is** caught correctly
(each `..` segment is rejected regardless of position). Same dormancy
caveat as N2 — not reachable via the current CLI, but load-bearing once
predicted paths come from a model.

### N4. Mission Control: `mission_id` doesn't bind `repo_id`

`crates/mice-cli/src/mission.rs:254-262`

`mission_id` is `slug(plan_display_name) + "-" + first-12-hex-of-sha256(plan
contents)`; `repo_id` is a separate sibling field on `MissionIdentity`, not
mixed into `mission_id` itself. The plan's invariant is "mission state must
be scoped by the Git common-directory hash **and** mission ID" (together).
No ledger/persistence exists yet in M0 (no `fs::write`/`SnapshotStore` use
anywhere in `mission.rs`), so this isn't exploitable today — but if a
future milestone ever uses `mission_id` alone as a storage key (directory
or file name) rather than the `(repo_id, mission_id)` pair, two different
repositories with an identically-worded plan file would collide. Worth
confirming any future persistence always keys by the pair.

### N5. `default_config_toml()`'s `Box::leak` — real per-call memory leak, currently test-only

`crates/mice-core/src/lib.rs:583-592`

The earlier `\\n`-vs-`\n` no-op bug (see "Fixed" section below) was
repaired by building the string at runtime via `.replace()` and then
calling `Box::leak(string.into_boxed_str())` to satisfy a `&'static str`
return type. Every call permanently leaks that allocation (~1.4 KB
currently) — `Box::leak` has no reclaim path by design. Grepped the whole
repo: the only call sites are the definition and one unit test
(`lib.rs:3138`), so this is harmless today. Flag for later: if this
function is ever wired into a runtime path (`mice init`, `mice setup`,
daemon startup config generation), it should return an owned `String`
instead.

---

## FIXED since the first pass (confirmed against current code)

- **Keychain empty-password save** (was Critical) —
  `save_keychain_api_key` (`main.rs`, near line 2812) now writes the secret
  to stdin twice and additionally checks stderr for "passwords don't
  match", rejecting the false-success case.
- **Reachable panic in `extract_json_object`** (was High) — now uses
  `let Some(...) = ... else { return value }` guards and computes the
  closing-brace offset relative to the found `{`, so it can no longer
  underflow/panic on out-of-order braces.
- **`mice ask` piped-stdin history leak** (was High) — `ask()` now branches
  on `text.is_some()` (piped source present) and calls
  `record_sensitive_history` (placeholder only) in that case, matching the
  pattern already used by `see()`/selection actions.
- **Escape killing an unrelated, concurrently-active Goal Guide** (was
  High) — `dismiss()` now only forwards Escape to `guidePanel` when the
  palette wasn't the visible surface (`paletteWasVisible` guard,
  `main.swift:2014-2030`).
- **Coordination mesh: rename-blind overlap detection** (was High) —
  `diff_hunks`/`classify_pair` now index renamed hunks under both the old
  and new path (test: `indexes_renamed_hunks_under_both_paths`,
  `coordination.rs:793`).
- **Coordination mesh: lockfile suppression hiding Red risks** (was Medium)
  — `should_suppress_risk` now requires `risk.level == RiskLevel::Yellow`
  before suppressing (`coordination.rs:598-600`); a genuine Red conflict on
  a lockfile is no longer hidden.
- **Coordination mesh: no timeout on `git` subprocess calls** (was Low) —
  `git_output_bounded` now enforces a 5-second deadline and kills the
  child on timeout (`coordination.rs:391-414`).
- **Coordination mesh: silently-dropped unparseable hunk headers** (was
  Low) — a malformed hunk header now surfaces as an explicit "unassessed"
  pair with a reason string instead of vanishing (`coordination.rs:236-239`).
- **Coordination mesh: unbounded O(n²) git spawning** (was Low) — capped at
  `MAX_WORKTREES = 16`; exceeding it short-circuits to "unassessed" instead
  of scanning (`coordination.rs:188-246,327`).
- **Palette truncation notice was dead code** (was Medium) —
  `response_budget()` now reserves headroom for `TRUNCATION_NOTICE` up
  front, so `finish()` can actually append it when truncation occurs
  (`main.rs:7670-7735`).
- **Palette: late response appended after stale timeout text** (was
  Medium) — `finish`/`timeout` now share a `timedOutSessionID` guard so a
  late `palette.result.finish` for an already-timed-out session is dropped
  rather than appended (`main.swift:1414-1423`).
- **`default_config_toml()`'s `.replace()` was a silent no-op** (was
  Medium) — fixed to use real `\n` escapes; the palette-trigger line is now
  actually injected (test at `lib.rs:3138`). See N5 above for the new,
  minor issue this fix introduced.
- **Palette reopen briefly reactivating the stale previous app** (was Low)
  — `dismissAndRestoreFocus` now takes a `restoreFocus` flag; the reopen
  path passes `false` so it no longer reactivates `previousApp` before the
  palette takes focus (`main.swift:1441-1457, 2053-2068`).
- **`mice home` posting a synthetic hotkey with no daemon listening** (was
  Low) — now gated on a real launch-time bridge-socket connect check
  (`homeHasResidentDaemon`, backed by `main.rs:269-272`). A small
  time-of-check/time-of-use gap remains by design (daemon could exit
  between Home launching and the button click) but this is a reasonable,
  documented tradeoff, not an unconditional post.
- **Other global gestures firing while the Palette has focus** (was Low) —
  the event tap's `keyDown`/`mouseMoved`/`flagsChanged` handlers all now
  check `OverlayController.isPaletteActive` first and pass through before
  reaching Smart Copy / hover / double-tap logic (`main.swift:194-280`).
- **`copy_directory()` copying symlinks as regular files** (was Low) — now
  detects symlinks via `file_type().is_symlink()` (which uses `lstat`, so
  it doesn't follow the link) and recreates them with `std::os::unix::fs::symlink`
  instead of `fs::copy`-ing their target's contents (`main.rs:310-334`).
- **AXI multi-line label confirmation context** (was Low) — `context` is
  now accumulated across physical lines via `quoted_context.join("\n")`
  instead of a single `line.trim()` (`tools.rs:100-155`). See N1 above: the
  fix is correct in intent but introduced a separate line-pairing bug.

## Checked, no bug found

- `parse_palette_intent` (`mice-core/src/lib.rs:691-722`): verb matching
  splits on the first whitespace run and compares the full token, so
  `"planet"` and `"seeing is believing"` correctly fall through to
  `Ask(...)` rather than misparsing as `plan`/`see` + remainder.
- `define term` precedence (`decisions.md`: "its typed term wins over any
  selection") — confirmed upheld at `main.rs:5749-5784`: the typed term is
  used first, selection text is only an `.or_else` fallback, and the
  result always dispatches `SelectionAction::Define`.
- `mission.rs` M0 scope discipline: no `fs::write`/`create_dir`/`remove`/
  `rename` anywhere; the only subprocess calls use fixed literal argv
  arrays (no plan-text ever reaches a shell); no `git worktree add/
  checkout/commit/branch/reset`; `--launch` is explicitly rejected by
  `MissionOptions::parse` (`mission.rs:135-140`); `tracked_worktree_cleanliness`
  falls through to `"unassessed (...)"` on any spawn/timeout error rather
  than defaulting to clean (`mission.rs:441`); `load_plan` canonicalizes
  both the `plan/` root and the requested path before a `starts_with`
  check, defeating symlink-escape attempts (`mission.rs:185`).
- Repo identity hashing (`coordination.rs:126-129,612-616`) uses a full,
  un-truncated SHA-256 of the canonical Git common-dir path — no
  truncation-collision risk; only display strings are shortened.

## Verification not run

No gates (`cargo fmt --check`, `cargo clippy`, `cargo test --workspace`,
`swift build`) were run as part of this review, and no source was
modified — this is an uncommitted working-tree diff being actively edited
by another agent.
