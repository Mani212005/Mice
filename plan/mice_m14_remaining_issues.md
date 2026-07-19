# M14 AXI guide — review status

> Follow-up review after the M13/M14 fixes in the current working tree.
> Supersedes the corrected M13 execution-manager and M13/M14 follow-up review
> notes. Updated after the implementation fixes landed.

## Verification

- `cargo fmt --check` — passed
- `cargo clippy --workspace --all-targets -- -D warnings` — passed
- `cargo test --workspace` — passed

## Resolved since this review was first written

- OTP-style `type=tel` fields whose accessible context suggests a code, PIN,
  or verification value now fail closed, with a regression test that verifies
  AXI is not invoked.
- Stale-reference recovery is scoped to each proposed action. A successful
  action does not consume a later action's retry, and the completed-action
  budget advances only after a dispatched action succeeds.

## Resolved after the review

- AXI local-lane selection calls `local_tool_model_available`, which requires
  an `ollama` executable, a supported configured tool model, and a reachable
  Ollama `/api/tags` response containing that model. `local_only` reports the
  remediation directly; `cloud_allowed` selects the confirmed cloud fallback
  instead of failing on the first model turn.

There are no remaining M14 correctness findings in this review. Generic
buttons without live form context remain an intentional safety handoff until
AXI exposes form-enriched snapshots.
