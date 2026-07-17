# MICE Plan v4 — M11: Guide-me that acts (browser executor + invisible extension)

> Extends plan v2 (Goal Guide) and reprioritizes plan v3: order is now
> **M11 → M7 → M8 → M9 → M10**. Planning document only; implementation follows
> the current branch settling.

## Context

M6a–M6c are complete: goal popup → reviewed plan → manual step panel, with
native AX highlights and (M6c) browser highlights through the token bridge.
Testing it against a real goal ("help me file my taxes") exposed two gaps:

1. **Guidance stops at pointing.** The user wants an escalation: tell me the
   steps → highlight where to click when I ask → and if I'm still stuck,
   **do that click/typing for me**. Decision (user-confirmed): MICE may act,
   but only **one action at a time, previewed, and executed only on the
   user's explicit "Do it" confirmation**. Login credentials, payment, and
   final-submission actions are never performed by MICE — highlight-only,
   per the standing decisions.md rule. Full hands-off autonomy is explicitly
   out of scope: current small/cheap models are not reliable enough for
   consequential flows, and the confirmation ladder delivers the "do it for
   me when stuck" experience without the risk.
2. **The extension experience is bad.** The popup is redundant now that the
   core drives guidance; the token/options dance is confusing; the user sees
   no value in the extension as a *thing they interact with*. Decision
   (user-confirmed): rework it into an **invisible, zero-config companion** —
   no popup, no token, no options page. Its only job is to be the Goal
   Guide's hands and eyes in the browser.

## Verified current state (2026-07-16, post-M6c)

- The bridge is an HTTP server on 127.0.0.1:9417 inside `mice start`
  (`run_browser_bridge`, `crates/mice-cli/src/main.rs:141`), token-gated via
  `MICE_BROWSER_BRIDGE_TOKEN`, with endpoints `POST /guide`,
  `POST /goal-step` (extension polls for the active step directive), and
  `POST /goal-highlight` (session-checked; reuses `guide_browser_request`).
- The extension background worker (`browser-ext/background.js`) *polls*:
  fetch directive → snapshot active tab → post snapshot → highlight the
  verified selector. Candidate-ID verification lives in Rust
  (`rank_guide_candidates`, `guide_browser_request`, main.rs:312–395).
- The content script (`browser-ext/content.js`) already: builds unique/
  structural selectors, snapshots bounded ranked candidates, and highlights a
  selector. It performs **no clicks or typing** today.
- decisions.md records the safety rule: execution must be confirmation-gated
  per step; no autonomous form submission, credentials, purchases, or
  sensitive account actions.

---

## M11a — Invisible zero-config extension (transport rework)

Replace the token/TCP bridge with **Chrome native messaging + a Unix socket
relay**:

1. `mice start` listens on a Unix domain socket
   (`~/Library/Application Support/MICE/bridge.sock`, mode 0600 — filesystem
   permissions replace the token; only the user's processes can connect).
2. New subcommand `mice native-host`: launched *by Chrome*, relays its
   stdio (Chrome's 4-byte length-prefixed JSON — same framing family as
   `mice-ipc`; reuse `read_frame`/`write_frame` with a native-endian header
   variant) to the core's Unix socket.
3. New subcommand `mice setup-browser`: writes the native-messaging host
   manifest to
   `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.mice.bridge.json`
   (absolute path to the `mice` binary + the extension ID). Add a `"key"`
   field to `browser-ext/manifest.json` so the unpacked extension ID is
   deterministic and the host manifest can be written once.
4. Extension: `chrome.runtime.connectNative` keeps a **persistent port** —
   the core now *pushes* goal-step directives (no more polling), and the
   extension answers with snapshots/results over the same port.
5. **Delete** `popup.html/js`, `options.html/js`, the token storage, and the
   `POST /goal-step` polling design. Keep an action badge only as a
   connected/disconnected indicator. The ad-hoc "guide me" popup flow is
   subsumed by the Goal Guide; `mice browser-bridge` (subcommand + token env
   var) is removed after M11a lands.

Result: install the unpacked extension once, run `mice setup-browser` once,
and the browser side of MICE never needs touching again.

## M11b — The action executor ("Do it" per step)

New verified-action protocol, extending the existing candidate-ID flow (the
model still never emits selectors; it picks candidates, Rust resolves them):

- **Wire (core ↔ extension over the M11a port):**
  `browser.act { sessionId, action: "click" | "fill" | "openUrl" | "scrollTo",
  candidateId?, url?, value?, previewText }` → response
  `{ ok, error?, pageChanged? }`.
- **Content script actions:** `click` = scrollIntoView + `element.click()`;
  `fill` = focus + native value setter + `input`/`change` events (works with
  React-controlled fields); both **re-verify the selector still resolves and
  is visible** before acting, else fail cleanly. `openUrl` runs in the
  background worker via `chrome.tabs.create`/`update` — this lets step 1 of a
  goal ("go to the tax portal") be performed for the user too.
- **Step panel (Swift):** browser-hinted steps gain a **Do it** button next
  to Next/Back/Quit. Flow: user presses Do it → core builds the preview
  ("MICE will click **'Continue'** on this page" / "open **irs.gov**") → the
  panel shows the preview with Confirm/Cancel → only on Confirm does the core
  send `browser.act`. **One action per confirmation, always.** Result (or
  failure) is reported in the panel; the user advances with Next as today.
- **Fill values are always user-supplied.** For type-something steps the
  panel shows a text field ("Type into *First name*:"); MICE never invents or
  stores personal values, and nothing typed is persisted (existing repo
  rule).
- **Plan schema:** goal-plan steps gain optional
  `browser_action { kind, target_hint }` so the planner can mark actionable
  steps; steps without it remain highlight/instruction-only.

## M11c — Safety layer (enforced twice)

Rust enforces, and the content script independently re-checks (defense in
depth — the model is never the safety mechanism):

- **Never fill** password fields (`type=password`), one-time-code fields, or
  payment fields (`autocomplete` beginning `cc-`).
- **Never click** targets whose label/type matches the sensitive blocklist
  (submit/pay/purchase/place order/confirm payment/file return/transfer,
  form-submit buttons inside payment or credential forms). These become
  highlight-only with the panel message "This one's yours — MICE highlights
  but won't press it."
- Plan steps flagged `sensitive: true` (existing M6a mechanism) never carry a
  `browser_action`.
- Every performed action is echoed to the terminal as an audit line
  (`[MICE act] clicked 'Continue' on tax.example.gov`); nothing is persisted.

## M11d — Stuck-flow polish (folds in old M6d)

- Panel buttons become: **Where?** (re-snapshot + re-highlight on the current
  page state), **Do it** (M11b), **Check me** (user-requested completion
  check: fresh bounded snapshot + the step's `done_check` to the model,
  answer shown — never auto-advances). Auto-advance stays out until the
  confirmed-action loop proves reliable.
- Native-app acting (AXPress via the Swift agent) is a later, separate
  decision — browser first, where verified DOM targets make acting safe.

---

## Files touched

`browser-ext/` (manifest key + nativeMessaging permission, background rewrite
to a persistent port, content-script `click`/`fill`, delete popup/options),
`crates/mice-cli/src/main.rs` (Unix-socket bridge replacing the TCP/token
server, `native-host` + `setup-browser` subcommands, Do-it flow, safety
blocklist), `crates/mice-ipc/src/lib.rs` (`browser.act` + step-panel button
types), `mice-providers` (plan schema `browser_action`), Swift step panel
(Do it/Where?/Check me buttons, preview + confirm, fill input field).

## Sequencing

M11a → M11b → M11c → M11d, then plan v3's M7–M10. Manifest and decisions.md
entries per milestone, per repo convention.

## Verification

- Standard gates: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace` (bridge/safety
  tests network-free), `swift build`, JS syntax checks.
- **M11a e2e:** `mice setup-browser`, reload the unpacked extension, run
  `mice start` with no token env var; badge shows connected; a goal with a
  browser step highlights without any polling delay and with zero manual
  extension configuration.
- **M11b e2e:** goal "open example.com and search for X" — Do it on the
  navigation step opens the tab after confirm; Do it on the search box shows
  the type-in field, fills it, and the page reflects the value. Cancel on the
  preview performs nothing.
- **M11c e2e:** on a test login form, verify password and "Sign in" targets
  are highlight-only with the refusal message; unit tests cover the blocklist
  and the content-script double-check (fill on `type=password` fails even if
  commanded).
- **M11d e2e:** Where? re-highlights after scrolling/navigation changed the
  page; Check me returns a sane verdict and does not advance the step.
