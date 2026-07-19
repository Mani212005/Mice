# M14 AXI guide — remaining review issue

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

## P1 — A selected local lane is not checked for an installed, usable model

`axi_model_lane` chooses `ExecutionLane::Local` from configuration and machine
profile, but neither it nor `autopilot_axi` verifies that Ollama is reachable
or that `config.tool_model` has a supported descriptor. The first local turn
then fails out of `call_axi_agent_turn` instead of using the documented
cloud-fallback consent path or producing the planned explicit local-lane
diagnostic. A standard-profile machine with no pulled tool model is sufficient
to hit this path.

**Required fix:** make local-lane eligibility include the same tool-model and
Ollama availability/preflight criteria as `mice bench-tools`; then either pause
with a clear remediation message or offer the already-confirmed cloud fallback.
Cover an unavailable Ollama/model in the AXI routing tests.
