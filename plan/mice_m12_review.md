# M12 Web Autopilot — code review (bugs & optimization)

> Reviewer notes on the landed M12 implementation. No source was changed.
> References are `file:line` against the working tree at review time.

## Resolution status (2026-07-17)

All findings below are now fixed and verified (`cargo fmt --check`, `clippy -D
warnings`, `cargo test --workspace`, `swift build`, `node --check` all pass;
the H0 bridge connection was confirmed live — the native host now stays
connected to the `mice start` daemon instead of "Connection refused").

- **H0** daemon/client bridge, **H1** page-ready signal (`tabs.onUpdated` +
  content.js startup `mice.page.ready`), **H2** wall-clock watchdog, **H3**
  per-action ack timeout, **H5** decision snake_case + raw-output logging,
  **M1** selector-keyed retry guard, **M2** Groq/no-OpenAI vision fallback,
  **M3** non-blocking overlay narration, **M4** submit over-block, **L3**
  merged history rows, and the **1 MB** screenshot compaction — completed
  earlier in the M12 work.
- **H4** non-injectable-tab escape, **H6#1** broadened candidate coverage
  (ARIA roles + bounded cursor:pointer sweep), and **H6#2** vision-on-stall
  trigger — completed in this pass. A snake_case decision-parse regression
  test is in `crates/mice-core` tests.

## Summary

The architecture is sound: the observe→decide→act loop is real, locks are
dropped before the multi-second cloud call (`advance_autopilot`,
crates/mice-cli/src/main.rs:365–383), candidate IDs are resolved locally so the
model never emits selectors, and the sensitive-action blocklist is enforced
**twice** (Rust `blocked_browser_action` main.rs:2302 and JS `act()`
browser-ext/content.js:164–196). The problems below are about the loop
*connecting* and *continuing* reliably on real pages, not about its shape.

---

## H0 — Native-host bridge never connects (why autopilot "does nothing")

**Confirmed by reproduction.** Launching the binary the way Chrome does —
`mice "chrome-extension://<id>/"` with a piped (non-tty) stdin — exits
immediately with `Connection refused (os error 61)`. That is exactly the
extension's flood of **"Native host has exited"** (804×) and **"Error when
communicating with the native messaging host"** (312×).

Mechanism:
- `native_host()` (main.rs:700) does `UnixStream::connect(bridge.sock)?` and
  **exits the instant nothing is listening**. The socket file persists on disk
  (created in `start_native_bridge` main.rs:188–193, never removed on exit),
  so a dead socket yields "Connection refused" rather than "not found."
- The listener exists **only while a one-shot `mice autopilot` (or
  `mice start`) runs** (main.rs:807, 1638/1779). The extension's background
  worker calls `connectNative` on a **1.5 s retry loop, independent of
  autopilot** (background.js:8–17), so every attempt while no listener exists
  spawns a host that dies → the error flood. `bridge.hello` never reaches the
  core, so autopilot sits forever at "Waiting for the MICE Browser Guide
  extension."

Compounding fragilities:
1. **Lifetime/ordering:** the socket is per-command, but the extension expects
   a persistent host; if the two aren't up at the same instant, nothing runs.
   Also, both `start()` (main.rs:1779) and `autopilot()` (main.rs:916) call
   `start_native_bridge`, and the second `remove_file`+`bind` silently steals
   the socket from the first — they cannot coexist.
2. **MV3 suspension:** after the failed-connect burst, Chrome can suspend the
   service worker, killing the `setTimeout(connect, 1500)` chain — so a later
   `mice autopilot` never gets a reconnect until the extension is manually
   reloaded.
3. **Stale socket** is never cleaned up on exit, so a fresh run can connect to
   a dead socket until `start_native_bridge` happens to remove it.
4. **1 MB extension→host cap:** the screenshot path posts a data URL up to
   ~4 MB (background.js:29, accepted to 4 MB main.rs:312), but Chrome limits
   **extension→host** messages to **1 MB** — this will drop the port mid-run on
   sparse/canvas pages even once the bridge works.

This H0 blocks everything below it: until the bridge connects, H1–H3 and the
whole loop are unreachable.

---

## HIGH — these stall the loop on the exact flows you tried

### H17. "Autopilot started. Observing…" printed even when Chrome was not connected (observed 2026-07-17)

Across restarts, `mice autopilot` connected to a daemon whose Chrome extension
was not connected (the native-host port was reconnecting), so the goal.step
reached no browser and nothing happened — yet the client printed "Autopilot
started. I am observing the current page.", which looked like it was working.
Fix: the `autopilot.start` handler now checks whether an extension client is
connected and reports honestly — either "Observing the current page." or
"Autopilot is ready, but Chrome is not connected yet … I will begin
automatically when it connects." The pending directive is already replayed by
the `bridge.hello` handler when the extension (re)connects, so a late connection
self-starts. (Client already errors clearly when no daemon socket exists.)
Workflow reminder: run `mice start` (daemon, owns the Chrome bridge and macOS
agent) and keep it running; run `mice autopilot "<goal>"` in a second terminal.

### H16. Duplicate-label candidates crowd the list and mislead the best guess (observed 2026-07-17)

Milestone run: MICE opened Canva (44 controls), the content script stayed alive,
and on handoff it rendered the on-page best-guess highlight — which the user
followed. But the highlight pointed at "Canva AI" because the diagnostic showed
`Canva AI | Canva AI | Canva AI | Canva AI | …`: the broadened cursor:pointer
sweep captured the sidebar button plus its nested icon/text/link, all labelled
"Canva AI", crowding out Custom size / Templates / the search box and making the
best guess (top candidate) wrong. Fix: `rank_guide_candidates` now dedupes by
visible label (keeping the first, highest-ranked occurrence) in addition to
selector, so distinct controls fill the list. Remaining limitation is model
judgment: a text-only model (Groq llama) tends to hand off/ask rather than
confidently pick "Custom size"; the screenshot-vision path (needs OPENAI_API_KEY)
is the real lever for the last mile inside a visual app.

### H15. Content script missing → "Receiving end does not exist", 0 controls (observed 2026-07-17)

Diagnostic showed `0 controls on canva.com` and act failures with
`Could not establish connection. Receiving end does not exist.` — the content
script was not responding in the tab. Two causes fixed: (1) content.js computes
`pageSignature` at import time via the heavy element scan; if that scan threw
(e.g. `getComputedStyle` returning null mid-mutation) the script aborted before
registering its message listener, so every request failed — `interactiveElements`
and `explicitControlCount` are now defensive (return empty on any error) so the
listener always registers; (2) right after a navigation the tab can report
`complete` before the content script's listener is ready — background.js now
retries `sendMessage` for snapshots and actions (4× / 250 ms) to ride out that
injection race. `reobserve=true` in the diagnostic confirmed the loop logic was
already correct; the gap was purely the content script's availability.

### H14. Re-observation after an action was conditional and could stall (observed 2026-07-17)

With one tab pinned, MICE clicked Canva's "Create a design" (`[MICE act]
completed`) but then stopped. `handle_autopilot_result` only re-observed when the
action reported `pageChanged:false`; on `pageChanged:true` it returned to waiting
for a separate `browser.pageChanged` event, which for a same-page-with-URL-change
interaction (mutations already fired, some gated by `!in_flight`) may never
arrive after the result — stall. Fix: **always re-observe after a successful
action** (the content-script settle-wait already handles page-load timing), and
in background.js **`open_url` now waits for the tab to reach `complete`** before
reporting success so the immediate re-observation sees a rendered page rather
than a blank/loading one. Added a `[MICE act-result] ok/pageChanged/reobserve`
diagnostic to make any future re-observation gap visible.

### H13. Multi-tab confusion — observes/acts on the wrong tab (general, observed 2026-07-17)

With other tabs open (a Google/YCombinator session), the diagnostic bounced
between `ycombinator.com`, `about:blank`, and `canva.com`, mostly `0 controls`,
and the model opened Canva twice thinking it wasn't loaded. Cause: `open_url`
created a **new tab** while snapshots/actions/screenshots all targeted
`{active: true}` and `onUpdated` forwarded page-changes for any active tab — so
MICE observed whichever tab happened to be focused, not the one it opened. Fix:
pin a single working tab per goal in background.js (`goalTabId`): `open_url`
navigates that tab in place (create only if none), snapshots/acts/highlights/
screenshots all target it, `onUpdated` and content-script page-change messages
are filtered to it (by `sender.tab.id`), it is released when the goal ends
(`goal.step` with null directive) and cleared on tab close. Also: `browser.act`
now reports a failure if the content script is unreachable (mid-navigation) so
the loop re-observes instead of hanging on a result that never arrives.

### H12. Click always reported as a navigation → loop stalls after same-page actions (observed 2026-07-17)

After the settle fix, MICE successfully clicked Canva's "Create a design" — a
panel opened — but the loop then stopped instead of continuing. `content.js`
`act()` returned `pageChanged: true` for **every** click; `handle_autopilot_result`
treats that as "a navigation is coming" and waits for a `browser.pageChanged`
event that never arrives for a same-page interaction (menu/dropdown/toggle keeps
the URL). Fix: report `pageChanged` only when `location.href` actually changed
after the click, so same-page actions re-observe the new state immediately (the
existing `!page_changed` branch resends the directive). General across sites —
any in-page panel/menu was affected.

### H11. Snapshot before the SPA renders → sparse observation + no highlight (general, observed 2026-07-17)

Diagnostic showed `6 controls on canva.com | top: Skip to main content | Skip to
header | Skip navigation …` — only the accessibility skip-links. MICE snapshotted
Canva **before its SPA finished painting**, so the real UI (search, Templates,
tiles) was absent; the model then reasoned "no clear option" and handed off with
nothing to highlight. General to any client-rendered app (the earlier run got
76–80 controls once loaded). Fixes:
- content.js `waitForSettle()`: before answering a snapshot, wait until the DOM
  is quiet for ~400 ms or a real control count (cheap explicit-selector count)
  crosses a threshold, capped at ~2 s. So snapshots reflect a painted page.
- On a handoff, always highlight: prefer the model's chosen control, else the
  best-ranked candidate as a clearly-labelled "best guess," so the user is
  pointed at something (the "it should have highlighted Templates" ask) even
  when the model does not select a control.

### H10. Observation too large → provider HTTP 413 (general, observed 2026-07-17)

With the extension reloaded, coverage worked (76–80 controls on Canva) but the
run failed with `Groq autopilot API failed: curl (22) error 413` (payload too
large). Canva's design-card tiles carry huge multi-line labels (whole card
contents), and the broadened cursor:pointer sweep also grabbed large container
divs, so the serialized observation exceeded Groq's request-size limit. The
observation was never size-bounded — a general problem on any control-dense
page. Fixes:
- Source (content.js): `cappedText` now collapses whitespace (multi-line blobs
  become one compact line), `MAX_SNAPSHOT_LABEL_CHARS` 240→140, and the pointer
  sweep skips elements whose text exceeds `MAX_POINTER_TEXT_CHARS` (200) — those
  are containers, not atomic controls.
- Router (main.rs): tightened `MAX_GUIDE_*` caps (candidates 80→60, label
  180→120, role 80→40, selector 1024→256) and added a hard
  `MAX_OBSERVATION_CHARS` (12 000) budget that keeps the highest-ranked
  controls that fit and truncates the rest; history entries are also bounded
  when rendered. This guarantees the request stays under the limit regardless
  of how verbose a page's labels are.

### H9. A single handoff prints 2–3× (false "loop"), and blind handoff needs vision (observed 2026-07-17)

After H8, a Canva run showed the same handoff sentence three times and looked
like a loop again — but it was **one turn**. Each turn emitted its message
through three paths: the up-front `say_to_user` narration, then the
`terminal_message` narration, then a separate `autopilot_status(done=true)` —
all the same text for a handoff. Fixed by consolidating: a single
`autopilot_narrate(text, done)` emits overlay + one client status with the
terminal flag; the up-front narration now runs only for non-terminal (action)
turns; the duplicate done-status is removed. One turn = one line.

The remaining *functional* problem is that the model handed off **blind** (no
control chosen) on Canva. Two general causes: (1) the vision escape (H6#2 and
the H7 pre-handoff screenshot) is gated on `OPENAI_API_KEY`, so a Groq-only
user (llama is text-only) never gets it; (2) if the extension is running an old
`content.js`, Canva's clickable-div tiles are not in the candidate list. Added
a daemon diagnostic — `[MICE observe] N controls on <url> | top: …` — so the log
distinguishes a poor snapshot (reload the extension) from a model/vision
limitation (needs an OpenAI key for the page-screenshot path).

### H8. Loop cadence not turn-based — re-decides before the action resolves (general, observed 2026-07-17)

After the H7 fixes, the loop still repeated "click Create a design" with no
`[MICE act]` line. **General root cause (not Canva-specific):** the loop
re-observes on every `browser.pageChanged`, and the content script's
`MutationObserver` fires that continuously on *any* dynamic/SPA site (Canva,
Gmail, React apps — every animation, lazy-load, async render). Because the
`in_flight` guard was released at *decision* time (before the dispatched
action's `browser.actResult` returned), each mutation immediately started a
fresh turn that re-decided the same action before the previous one resolved.
The click was sent, but its result was buried under the flood of
re-observations.

Fix (site-agnostic, makes the loop strictly turn-based): hold `in_flight` from
the start of a turn until the action's **result** is processed, not until the
decision is made. Concretely — release the guard only in `handle_autopilot_result`
(actResult arrived) and the ack-timeout watchdog path; keep it held across
dispatch; and gate the `browser.pageChanged` handler on `!in_flight` so a busy
page cannot consume the awaited post-action page-change. Result: exactly one
observe → decide → act → result cycle at a time regardless of a site's DOM
mutation rate. The ack timeout still bounds a lost result, and the stuck→vision
escalation still handles an action that produced no visible change.

### H7. Blind handoff with no highlight, and a repeating handoff loop (observed 2026-07-17)

Live run reached Canva, then repeated "the current page does not provide a
clear option… let's hand off to the user" several times and highlighted
nothing. Three problems, all now fixed:

1. **No highlight on a generic handoff.** The model handed off without picking a
   candidate, so `highlight_to_send` (only set when a candidate is selected)
   stayed empty — the user got no highlight. Fix: when the model is about to
   `handoff`/`ask_user` with no candidate and vision is available, first request
   a screenshot and retry the turn once (`advance_autopilot`), so it can see the
   control the DOM omitted and either click it or hand off *pointing at it*.
2. **The handoff looped.** A page-heavy SPA (and MV3 service-worker churn) can
   deliver a burst of observations; without mutual exclusion the loop ran
   several model turns in parallel and narrated/handed off repeatedly. Fix: an
   `in_flight` guard on `AutopilotRun` drops observations that arrive while a
   turn is being computed, and terminal states now **tear the run down**
   (`state.autopilot = None`) so a late/duplicate observation cannot re-enter.
   Post-teardown observations are silent no-ops (no spurious "I ran into a
   problem").
3. **Coverage only helps if the extension is reloaded.** The "no clear option"
   handoff indicates the browser was still running the pre-H6#1 `content.js`
   (narrow selector), so Canva's clickable-`div` tiles never reached the
   candidate list. The extension MUST be reloaded in `chrome://extensions`
   after any `content.js`/`background.js` change — the recompiled Rust daemon
   does not reload the browser side.

### H6. Canva-class stall — missing candidates + stale clicks + no graceful handoff (observed 2026-07-17)

Live run after H5-class fix: the loop navigated google → canva.com, scrolled,
tried to click a "Create a design" button (→ `Target is no longer visible`),
then repeated "…let's try searching for 'portrait' or looking for a 'custom
size' option" three times without acting, never clicking the **visibly present**
"Custom size" tile. Three compounding causes:

1. **Candidate coverage gap (root cause).** content.js snapshots only
   `a,button,input,select,textarea,[role='button'],[role='link']`
   (browser-ext/content.js:92). Canva's home tiles ("Custom size", "Doc",
   "Whiteboard", the "+ Create" control) are clickable **divs/spans without a
   button/link role**, so they are **absent from the candidate list** — the
   model can describe what it sees but literally cannot select it, so it
   deliberates in a loop. Fix: broaden the query to include `[onclick]`,
   `[tabindex]`, `[role=menuitem]`, `[role=option]`, and elements with computed
   `cursor:pointer` (dedup by nearest clickable ancestor to avoid flooding).

2. **Vision fallback never triggers on busy pages.** A screenshot turn is only
   requested when `candidates.len() <= 3` (main.rs:487–488). Canva home has
   many links, so it stays > 3 and vision never engages — even though the
   *relevant* control is the one missing from the DOM list. Fix: also trigger a
   screenshot turn when the loop is **not progressing** (e.g. ≥2 turns with no
   page change / repeated narration), not only when candidates are sparse.

3. **Stale click + no handoff (M1 again).** The click failed with "Target is no
   longer visible" (Canva's SPA re-renders between decide and act). Because the
   retry guard keys on the per-snapshot `candidate_id` (see M1), repeated
   failures on the same visual target don't reach the 2-failure handoff, so it
   loops instead of degrading to "I've highlighted Custom size — please click
   it." Fixing M1 (key on selector/label) converts this exact dead-loop into
   the human-in-the-loop handoff the product wants.

Product note: the user's helper vision (guide a first-time tax filer; act when
able, hand off with a highlight when stuck) is served most by #3 — a reliable
handoff path — plus #1 so the agent can actually see the controls. These three
are the highest-value follow-ups now that the loop runs end to end.

### H5. Decision JSON casing mismatch — every turn fails to parse (observed 2026-07-17)

Reproduced live from a google.com tab: the loop started, snapshotted, called the
cloud model, then printed "The cloud model returned an invalid autopilot
decision." and stopped. The model call **succeeded**; the *parse* failed.

`AgentDecision` (crates/mice-core/src/lib.rs:264) is
`#[serde(rename_all = "camelCase")]`, so deserialization requires JSON keys
`sayToUser`, `candidateId`, `doneSummary`, `question`. But both provider
payloads emit **snake_case**: the OpenAI strict schema properties are
`say_to_user`/`candidate_id`/`done_summary` (mice-providers/src/lib.rs:374–379,
required list snake_case) and the Groq system prompt specifies the same
(line 407). The model returns `say_to_user`; serde looks for the required
`sayToUser`, fails with "missing field", and `advance_autopilot`'s
`serde_json::from_str(&output).map_err(|_| "…invalid…decision")` (main.rs:437)
discards the real error. **Every turn fails, for both providers** — autopilot
cannot complete a single decision.

Fix: change `AgentDecision`'s `rename_all` to `"snake_case"` (the struct is the
outlier; both payloads and the plan v5 schema already use snake_case). Also
replace the `map_err(|_| …)` with one that logs the raw model output and the
serde error to stderr so future decode failures are diagnosable. Add a unit
test that parses a representative snake_case model JSON into `AgentDecision`
(the existing tests only build the struct in Rust, so they never exercise the
wire casing).

### H4. A non-injectable active tab stalls the loop at start (observed 2026-07-17)

Reproduced live: with `mice start` (daemon) running and `mice autopilot "go to
Canva…"` in a second terminal, the client printed "Autopilot started. I am
observing the current page." and then nothing. The active Chrome tab was
`chrome://settings/content/siteDetails?site=chrome-extension://…`.

Cause: on `autopilot.start` the daemon sends `goal.step`; background.js
(browser-ext/background.js:93–101) does
`chrome.tabs.sendMessage(activeTab, "mice.guide.snapshot")`. **Content scripts
never run on `chrome://` pages** (also the New Tab page, the Web Store, PDFs,
`view-source:`), so `sendMessage` rejects, `handleCoreMessage` throws into the
queue `.catch` (background.js:42) and **no `goal.snapshot` is ever posted**.
`advance_autopilot` is only reachable from `goal.snapshot` (main.rs:342–356), so
the loop never takes its first turn — not even the `open_url` that would move it
to a real page. The daemon *does* log a `browser.pageChanged` for the
chrome:// URL (via the new `tabs.onUpdated` hook, background.js:122), but on
start `awaiting_page_change` is false so that event doesn't advance anything.

This is a permanent stall whenever autopilot is launched from any
non-http(s) tab. Fix direction: when the active tab isn't injectable (or the
snapshot throws), background.js should still post a `goal.snapshot` with empty
`elements` and the tab URL, so the daemon can run `advance_autopilot` with an
empty candidate list and the model chooses `open_url` to reach a real page.
(Operational workaround today: start autopilot from a normal website tab.)

Current-state confirmations from this review pass: the daemon/client split
(Part 1) has landed — `mice start` owns the socket, `autopilot.start/stop/status`
control frames exist (main.rs:280–329), the double-bind is gone; careful-mode
is now non-blocking overlay narration rather than a stdin gate (main.rs:688–692,
so M3's terminal-confirm concern is resolved); the `tabs.onUpdated` page-ready
hook (H1) and screenshot compaction under ~900 KB (H0/4, 1 MB cap) are present.

### H1. A full page navigation never resumes the loop (the Canva-flow killer)

After a click/open_url that succeeds, `handle_autopilot_result`
(main.rs:594–641) sees `ok && pageChanged == true` and returns `None`,
**leaving `awaiting_page_change = true`** and waiting for a
`browser.pageChanged` message to re-send the `goal.step` directive
(main.rs:326–351).

That message only comes from `publishPageChange` in content.js
(browser-ext/content.js:127–142), which fires on **MutationObserver /
pushState / popstate within an already-loaded document**. On a **top-level
navigation** (click a Google result → canva.com, or `open_url`), the old
document is destroyed and a **fresh content.js** is injected; its module-scope
`pageSignature` (content.js:125) initializes to the new page, so **no delta and
no `mice.page.changed` event is ever emitted for a freshly loaded page**.
content.js:211 calls `publishDomSnapshot()`, which only dispatches an in-page
`CustomEvent` nothing listens to.

Result: the autopilot advances fine *within* an SPA but **stalls the moment it
crosses a real page load** — precisely step 2 of "search Canva → click the
result → open canva.com." Likely the main cause of "not even remotely close."

Fix direction: emit a "page ready" signal on load — background.js listens to
`chrome.tabs.onUpdated` for `status === "complete"` on the active tab and
re-sends the directive (covers full loads and `chrome.tabs.create`), with
content.js optionally posting one `mice.page.changed` at startup.

### H2. The "15-minute safety cap" is not actually a timer

The wall-clock cap is only checked *inside* `advance_autopilot`
(main.rs:371–375), which runs only when a message arrives. The `autopilot()`
main loop just sleeps while `Running` (main.rs:811–818) with no timeout. So
whenever the loop stalls for lack of an inbound message (H1/H3), nothing
enforces the cap and the CLI **hangs indefinitely** until Ctrl-C. Fix: a
watchdog thread that stops the run after the cap regardless of traffic.

### H3. MV3 service-worker death loses in-flight actions

MV3 kills the background worker after ~30 s idle. `connect()` on disconnect
retries (background.js:11–14) and re-sends `bridge.hello`, so the port recovers
— but any `browser.act` already sent to the dead port is lost, its
`browser.actResult`/`pageChanged` never arrive, and the loop stalls (unbounded
per H2). Fix: a per-action ack timeout in the core that re-observes if no
`actResult` arrives within N seconds, so a dropped worker self-heals.

---

## MEDIUM

### M1. Retry-guard keys on an unstable ID

`record_action_result` (mice-core/src/lib.rs:357–377) detects "failed twice on
the same target" via `last_action_target`, which is the `candidate_id`
(`"candidate-3"`, main.rs:508). Candidate IDs are **regenerated by rank order
on every snapshot**, so the same string can point to different real controls
across turns (false "second failure" → premature handoff) or the same control
gets a new ID (real repeat failure missed). Key on the resolved `selector` or
`label`.

### M2. Vision turns hard-require OpenAI even in a Groq-only setup

When the DOM is sparse and a screenshot is used, the turn is hard-coded to
`gpt-5.6-sol` via `call_openai_agent_turn` (main.rs:410–418), needing
`OPENAI_API_KEY`. A Groq-configured user with only `GROQ_API_KEY` gets text
turns via Groq but **sparse/canvas turns fail** rather than degrading. Detect
the missing key and fall back to a text-only turn on the configured model with
a spoken notice.

### M3. `careful_mode` defaults on with no graduation → double gating

`default_autopilot_careful_mode` is `true` (mice-core/src/lib.rs:59). Combined
with the once-per-goal consent prompt (main.rs:749–756), the out-of-the-box
experience prompts `y/N` for **every** action (`confirm_autopilot_action`,
main.rs:583–592). The v5 plan intended per-action confirm only for the *first*
run. Also `confirm_autopilot_action` reads stdin on the **bridge reader
thread**, so the terminal — not the native overlay — is the confirmation
surface, which the elderly-user UX wanted to avoid. Consider graduating
careful mode after one completed goal and moving confirm to the overlay/client.

### M4. Over-broad sensitive-click match blocks benign submits

Both blocklists match the bare substring `submit` (main.rs:2323,
content.js:178). A search/filter button that is `type="submit"` or labeled
"Submit search" is forced to handoff even on a non-sensitive form. The JS side
already has the good signal (`type === "submit"` inside a form containing
password/cc/otp inputs, content.js:179); the standalone keyword is what
over-blocks and can stall a search step mid-task. Tighten to form-context.

---

## LOW / optimization

- **L1. Screenshot payload is heavy.** `captureVisibleTab` JPEG q55
  (background.js:29) can be hundreds of KB → multi-MB base64, accepted to 4 MB
  (main.rs:312). No downscale. Slower and pricier; also collides with the 1 MB
  extension→host cap (H0/4). Cap dimensions or lower quality.
- **L2. Ranking uses the whole goal string** every turn (main.rs:385), so
  candidate ordering doesn't track the current sub-intent. Ranking against the
  last narration / recent history would surface the relevant control higher
  within the 80-cap.
- **L3. Two history rows per action.** Each action records a dispatch row
  (main.rs:510) and a result row (`record_action_result` → `record`), and
  history is capped at 15 (mice-core:350), so effective memory is ~7 actions.
- **L4. Overlay jumps.** Each turn calls `OverlayShow` (native_overlay
  main.rs:223), which repositions the panel to the mouse; narration hops
  around. A "narration update" that keeps position reads calmer.
- **L5. `open_url` destination is unrestricted** (any http/https). A goal-domain
  allowlist would harden it.
- **L6. Extension `handleCoreMessage` is async with no serialization**
  (background.js:19); concurrent core messages could interleave tab calls.
  Latent (the core serializes sends today), not active.

---

## Suggested fix order

1. **H0** (persistent-daemon bridge) — nothing else is observable until the
   bridge connects.
2. **H1** (page-ready signal), then **H2** (watchdog) + **H3** (ack timeout) —
   turn stalls into clean, bounded stops/recoveries.
3. **M1**, **M2**, **M4**, then M3 UX and the L-series.

The approved implementation plan for these is
`plan/mice_m12_fix_bridge_and_stalls.md`.
