# M13 execution manager — review findings

> Review of the M13/M15 execution-manager implementation at `b9775ef`.
> No implementation source was changed. References are against that revision.

## Verification completed

- `cargo fmt --check` — passed
- `cargo clippy --workspace --all-targets -- -D warnings` — passed
- `cargo test --workspace` — passed (59 tests)

The passing suite does not exercise the failure and safety paths below.

## P0 — Browser credential guard accepts ordinary secrets

`is_sensitive_browser_call` rejects only `browser.fill` arguments whose
**uid or text literally contains** a short list of words
(`crates/mice-cli/src/tools.rs:324-346`). Browser control UIDs are normally
opaque, and a password, OTP, or card value does not contain strings such as
`password`, `otp`, or `cvv`. For example, filling uid `g7:input2` with
`Tr0ub4dor&3` reaches `chrome-devtools-axi` unblocked.

This contradicts the advertised guarantee that AXI autopilot never enters
credentials, OTPs, or payment data, and it lets an MCP client or the local
model exfiltrate secrets through the browser adapter.

**Required fix:** establish sensitivity from trusted, current browser metadata
(input type/autocomplete/associated label) in the adapter, and reject before
the value is passed to a subprocess. Do not infer field sensitivity from the
secret value or an opaque uid. Add tests for opaque password, OTP, and payment
uids with realistic values.

## P0 — Raw browser actions bypass the M12 sensitive-action policy

The M13 registry exposes `browser.click` and `browser.press` as ordinary
mutating commands (`tools.rs:202-225`, `tools.rs:392-410`). Apart from the
optional global `careful_mode` rejection (`tools.rs:286-290`), they have no
validated snapshot provenance, target label/role, or sensitive-action check.
The manager can therefore click a login, payment, transfer, purchase, or final
submission control, or press Enter to submit one. This bypasses the stricter
M12 candidate policy and the double-enforced action blocklist documented in
`manifest.md`.

**Required fix:** do not grant an SLM/MCP caller raw browser mutation commands.
Keep a short-lived, trusted snapshot of allowed targets and validate every
action's current target category before dispatch; require an explicit
confirmation protocol for permitted mutations. Cover login/payment/transfer/
purchase/final-submit click and Enter paths with tests.

## P1 — Live read-only tools are cached indefinitely from unrelated Git state

Every `ReadOnly` tool is cacheable (`main.rs:1864-1867`), but its key varies
only with the local repository HEAD/dirty string (`main.rs:1819-1840`,
`main.rs:1860-1868`). Consequently, a `browser.snapshot`, GitHub PR/issue
query, or `quota.status` result is returned from a prior run while the browser,
remote GitHub state, or quota has changed. A browser loop can act after a new
page state and then reason from the stale pre-action snapshot.

**Required fix:** add per-tool cache semantics. Browser snapshots and quota
should not be persisted as cache entries; remote tools need an explicit short
TTL or a provider version/ETag; repository-local tools may use a repository
fingerprint. Add an invalidation test that changes each live source while Git
state remains unchanged.

## P1 — Cache and macro filenames collide and can exceed filesystem limits

Artifact and macro paths are produced by replacing every non-alphanumeric
character with `_` (`memory.rs:98-117`, `memory.rs:120-140`, `memory.rs:318-
329`). Distinct keys such as `a/b` and `a?b` therefore map to the same file,
so one request can receive another request's cached output or replay its macro.
In addition, an artifact key embeds the complete `git status --porcelain`
output (`main.rs:1819-1840`) and unbounded arguments, so a dirty repository
with many paths can produce a filename longer than the platform component
limit; the otherwise successful read-only tool then fails while caching.

**Required fix:** use a fixed-length cryptographic digest for storage names,
and retain/verify the original canonical key inside the stored record. Reject
or bound oversized input fields before persistence. Add collision and long-
dirty-worktree tests for both artifacts and macros.

## P1 — The artifact cache persists browser captures and other raw tool data

`run_registered_tool` copies the full stdout into `CachedArtifact::raw`
(`main.rs:1901-1908`) and writes it under
`~/Library/Application Support/MICE/memory` (`memory.rs:73-76`, `memory.rs:98-
104`). Since `browser.snapshot` is marked read-only, its full accessibility
capture is persisted automatically. GitHub output can likewise contain private
issue/PR content. This conflicts with the repository rule and manifest that
captures must not be persisted.

**Required fix:** never persist browser snapshots/captures; keep such output
in memory only for the active turn. Revisit raw-output persistence for all
tools, storing only non-sensitive bounded metadata where an artifact is
genuinely required. Add a test asserting that a browser snapshot creates no
artifact file.

## P1 — Workflow macros replay browser mutations without revalidation or consent

On an exact goal-string match, `delegate_task` immediately replays every stored
call (`main.rs:2141-2180`). Macros are saved after a completed local-model run
(`main.rs:2112-2115`, `main.rs:2206-2207`) and can include `browser.open`,
`browser.click`, `browser.fill`, `browser.press`, and `browser.scroll`.
The replay neither captures a fresh page nor asks for consent before dispatch,
yet labels it a “verified local workflow.” A changed page can turn a formerly
safe uid/action sequence into an unrelated destructive action.

**Required fix:** restrict replay to demonstrably idempotent read-only tools;
otherwise require a fresh validated snapshot and explicit per-action consent.
Include a regression test proving a macro with a browser mutation cannot run
automatically.

## P2 — Local tool loops are unbounded by the advertised safety budget

`mice do --max-actions` accepts any `usize` (`main.rs:2063-2067`), and MCP
`delegate_task.max_actions` accepts any JSON `u64` then casts it to `usize`
(`main.rs:4251-4258`). Neither path limits the value, although the interface
describes the task as bounded. A caller can cause an effectively unbounded
sequence of model calls and browser/tool actions; on 32-bit platforms the cast
also truncates large values.

**Required fix:** centralize a small hard maximum, reject zero/out-of-range
values before starting, and use checked conversion for MCP input. Test both CLI
and MCP validation at the lower and upper bounds.

## P2 — Shared-memory appends are not safe for concurrent agents

`SharedMemory::append` performs two independent append operations followed by
a derived-state rebuild (`memory.rs:78-92`). There is no inter-process lock,
and an event is serialized directly into the shared file before a separate
newline write. Concurrent MCP servers can interleave serialized bytes or read
while another process is mid-append; a malformed line then makes `events()`
fail during every subsequent rebuild. This defeats the stated multi-agent
coordination role.

**Required fix:** serialize each JSONL event into one buffer, guard the shared
append plus derived rebuild with an inter-process lock, and publish derived
files atomically. Add a multi-process/concurrent-writer regression test.

