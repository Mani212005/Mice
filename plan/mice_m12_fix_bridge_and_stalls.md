# M12 Autopilot — fix the bridge + the loop stalls (M12-fix)

> Approved implementation plan (user-approved 2026-07-17). Companion to the
> review in `plan/mice_m12_review.md`. Reviewer/checker: this document is the
> spec for the implementing agent; it is not implemented here.

## Context

`mice autopilot "go to canva and make a portrait"` sits at "Waiting for the
MICE Browser Guide extension" and never acts. The Chrome extension console
shows **"Native host has exited" (804×)** and **"Error when communicating with
the native messaging host" (312×)**. This is a **transport/wiring failure, not
the autopilot logic and not API credits** — the observe→decide→act loop never
runs because the browser bridge never connects.

**Root cause (reproduced):** launching the binary exactly as Chrome does —
`mice "chrome-extension://<id>/"` with a piped stdin — exits immediately with
`Connection refused (os error 61)`. `native_host()`
(crates/mice-cli/src/main.rs:700) connects to the Unix socket and **exits the
instant nothing is listening**. The socket only exists *while* a one-shot
`mice autopilot`/`mice start` runs, but the extension's background worker calls
`connectNative` on a 1.5 s retry loop **independent of autopilot**
(browser-ext/background.js:8–17). Every attempt with no listener spawns a host
that dies → the error flood → `bridge.hello` never reaches the core → autopilot
waits forever. The manifest, binary path, extension ID, and socket file are all
correct on disk; only the lifetime model is wrong.

Decisions locked with the user: **persistent daemon owns the socket**; deliver
**one combined plan** that also fixes the deeper loop stalls found in review.
Full findings (H0–H3, M1–M4, L1–L6) are in `plan/mice_m12_review.md`. This plan
implements H0 + H1/H2/H3 + the 1 MB cap + the highest-value mediums. Feature
plans M7–M10 remain in `plan/mice_planv3_files_smartcopy_agents.md` and are
unaffected.

---

## Part 1 — Persistent daemon bridge (fixes H0)

Make `mice start` the sole, resident owner of the bridge socket; turn
`mice autopilot <goal>` into a thin control client that hands its goal to the
running daemon. This matches plan v1's "runs resident" design and removes the
lifetime/ordering race, the double-bind conflict (today both `start()`
main.rs:1779 and `autopilot()` main.rs:916 call `start_native_bridge`, and the
second `remove_file`+`bind` silently steals the socket from the first), and the
MV3 reconnect fragility (the daemon socket is always present, so the extension
connects once and stays).

Changes:

1. **Socket ownership + cleanup.** `start_native_bridge` (main.rs:195) stays,
   but only `mice start` calls it. Remove the stale socket on clean exit
   (install a Drop guard / signal handler that unlinks `bridge.sock`), and if
   `bind` fails with `AddrInUse`, probe-connect first: if a live daemon
   answers, tell the user one is already running; if it's stale, unlink and
   rebind.

2. **Daemon hosts autopilot.** Move the `AutopilotRun` lifecycle
   (main.rs:162–168, 359–581, 594–698) into the `start()` process. `start()`
   already owns a native overlay agent (main.rs:1776+) and the extension
   client, so the loop reuses both for narration and DOM I/O — no second agent,
   no second socket.

3. **Control channel over the same socket.** `handle_native_bridge`
   (main.rs:227) already multiplexes by `message["type"]`. Add control
   messages from a *CLI* client (distinct from the Chrome native host):
   - `autopilot.start { goal }` → daemon builds `AutopilotRun`, sets the
     directive, kicks the extension (`goal.step`).
   - `autopilot.stop` → `stop_autopilot`.
   - Daemon → client status frames (`autopilot.status { text, done }`) so the
     CLI can print narration/handoff/consent lines to the terminal and exit
     when the run ends.

4. **`mice autopilot <goal>` becomes a thin client** (rewrite of main.rs:733):
   connect to `bridge_socket_path()`; if connect fails, print
   "Start the daemon first: `mice start`." Keep the once-per-goal consent
   prompt here (main.rs:745–756). Send `autopilot.start`, stream status frames
   to stdout, and on Ctrl-C send `autopilot.stop`. No socket, no agent, no
   bridge in this process anymore.

5. **Careful-mode confirmation moves off the daemon's missing stdin.** The
   daemon has no terminal, so per-action confirm (main.rs:583) can't
   `read_line` there. Route the confirm to the **CLI client** via an
   `autopilot.confirm { preview }` request/response frame (client prints the
   prompt, reads y/N, replies). This also fixes review item M3's "confirmation
   should not live in the terminal-only path" and keeps careful mode usable.

---

## Part 2 — Keep the loop moving (H1, H2, H3, 1 MB cap)

6. **H1 — resume after a full page navigation (the Canva-flow killer).** After
   a click/open_url, the core waits for `browser.pageChanged` before the next
   turn (main.rs:326–351, 594–641), but a **top-level navigation** loads a
   fresh content.js whose `pageSignature` (browser-ext/content.js:125)
   initializes to the new page with no delta, so **no page-changed event ever
   fires** and the loop stalls. Fix: in **background.js**, listen to
   `chrome.tabs.onUpdated` for `status === "complete"` on the active tab and
   post `browser.pageChanged` — this covers full loads and new tabs from
   `chrome.tabs.create`. (Belt-and-suspenders: content.js may also post one
   `mice.page.changed` at startup.)

7. **H2 — real watchdog.** The 15-min cap is only checked inside
   `advance_autopilot` (main.rs:371), so a stalled loop hangs indefinitely.
   Add a watchdog thread in the daemon that calls `stop_autopilot` once
   `started_at.elapsed() > AUTOPILOT_WALL_CLOCK_CAP` regardless of traffic, and
   emits an `autopilot.status { done }` so the client exits.

8. **H3 — per-action ack timeout.** If the MV3 worker dies mid-action, the
   `browser.actResult` never returns and the loop stalls. Track the pending
   action's dispatch time; if no result within N seconds (e.g. 20), record a
   failure and re-observe (re-send the directive) rather than waiting forever.
   The extension already auto-reconnects (background.js:11–14), so re-observe
   self-heals a worker restart.

9. **1 MB extension→host cap.** The screenshot data URL can reach ~4 MB
   (background.js:29, accepted to 4 MB main.rs:312), but Chrome caps
   **extension→host** messages at **1 MB** and will drop the port. Fix in
   background.js: capture at lower resolution/quality and, if still over
   ~900 KB, downscale via an `OffscreenCanvas` before posting; lower the Rust
   accept ceiling (main.rs:312) to match. Keeps canvas-page vision (the Sheets
   scenario) from killing the bridge.

---

## Part 3 — High-value correctness bundled in (M1, M2, M4)

10. **M1 — stable retry key.** `record_action_result` keys the "failed twice"
    guard on `candidate_id` (mice-core/src/lib.rs:357, main.rs:508), but
    candidate IDs are regenerated per snapshot, so the guard misfires. Key on
    the resolved **selector** (or label) instead.

11. **M2 — Groq vision fallback.** Sparse-page turns hard-require OpenAI
    (`gpt-5.6-sol`, main.rs:410–418). If the user configured a Groq cloud model
    and set only `GROQ_API_KEY`, detect the missing `OPENAI_API_KEY` and fall
    back to a text-only turn on the configured model with a spoken notice,
    instead of erroring the run.

12. **M4 — stop over-blocking benign submits.** Both blocklists match the bare
    word `submit` (main.rs:2323, content.js:178), which forces handoff on
    ordinary search/filter buttons mid-task. Restrict the click block to
    `type="submit"` **inside a form containing password/cc/otp inputs**
    (content.js:179 already computes `hasSensitiveForm`) plus the explicit
    pay/transfer/file-return phrases; drop the standalone `submit` keyword.

---

## Files touched

- `crates/mice-cli/src/main.rs` — daemon/client split, control + confirm
  frames, socket cleanup, watchdog, ack timeout, retry key, Groq-vision
  fallback, screenshot ceiling, submit blocklist.
- `browser-ext/background.js` — `tabs.onUpdated` page-ready signal, screenshot
  downscale under 1 MB.
- `browser-ext/content.js` — optional startup page-change post; submit
  blocklist tightening.
- `crates/mice-core/src/lib.rs` — `record_action_result` keyed on selector.
- No IPC wire-type changes beyond the new control/confirm/status frames (add
  them in `mice-ipc` per the no-duplicated-wire-types rule).

## Verification

1. **Bridge connects (H0).** `mice start` (daemon) running; reload the
   extension; badge shows connected; **no more "Native host has exited" flood**.
   Confirm by re-running the Chrome-launch reproduction:
   `printf '' | mice "chrome-extension://pmbogcpjmddjpgcilhiplppdhnboeofc/"`
   now blocks (connected) instead of printing "Connection refused".
2. **Thin client.** With the daemon up, `mice autopilot "…"` prompts consent,
   streams narration to the terminal, and exits on done/handoff; with the
   daemon down it prints the "start the daemon first" hint.
3. **Canva flow (H1).** Goal "go to canva and make a portrait": the loop now
   advances **past** the Google-result click / canva.com load instead of
   stalling — the headline behavior change.
4. **Stalls bounded (H2/H3).** Kill the extension mid-run → the loop
   re-observes or stops within the ack/watchdog window rather than hanging.
5. **Vision cap.** On a canvas page (Google Sheets), the screenshot turn posts
   under 1 MB and the port stays connected.
6. Standard gates: `cargo fmt --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace` (control/confirm framing + selector-keyed retry
   guard get unit tests; keep network-free), `swift build`, JS syntax checks.

## Sequencing

Part 1 first (nothing else is observable until the bridge connects), then
Part 2 (H1 is the next thing you'll hit on the Canva flow), then Part 3. After
this, resume plan v3 (M7–M10).
