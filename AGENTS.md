# MICE Contributor Guidance

## Identity & Prime Purpose

MICE is a privacy-first, terminal/TUI sidekick sub-agent designed to pair with orchestrator LLMs (such as Gemini 3.7 Flash, Claude Code, Antigravity, and Cursor).
It executes token-heavy routine engineering tasks locally (semantic document retrieval, AST symbol searches, code patching, local test runs) and returns compact, verified diffs and structured summaries.

## Delivery Order

1. Keep portable Rust core (`crates/mice-core`) independent of terminal frontends or platform-specific shells.
2. Maintain sub-millisecond retrieval guarantees for `SemanticFinder` and in-memory `KnowledgeGraph`.
3. Do not persist credentials, captures, clipboard contents, model weights, or user configuration in this repository.

## Architecture Boundaries

- For basic architecture boundaries, see [README.md](README.md).
- Add or change wire protocol types in `crates/mice-ipc`.
- Sidekick task types and token metrics live in `crates/mice-core/src/sidekick.rs`.
- Interactive terminal dashboards live in `crates/mice-cli/src/sidekick_cli.rs` using Ratatui.
- MCP tool endpoints are defined in `crates/mice-cli/src/main.rs`.

## Verification

Run the complete verification pipeline before committing changes:
```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Multi-Agent Handoff Protocol

Two agents work this repository - Claude Code and Antigravity - taking turns on the same branch.

**At the start of every session:**
1. Run `git log --oneline -10` to see recent work.
2. Read `.agents-sync/handoff.md` if it is non-empty.

**Before ending a session, if the task is not fully resolved:**
Overwrite `.agents-sync/handoff.md` with the template below.

```markdown
# Handoff - [timestamp]
**From:** Claude Code | Antigravity
**Status:** stuck / in-progress / resolved
**Branch:** feat/sidekick-tui-agent

## Goal
One sentence: what bug/task are we solving.

## What I tried
- Attempt 1: [what], result: [what happened]
- Attempt 2: [what], result: [what happened]

## Current state of the code
- Files touched: `crates/...`
- What currently works / what is still in progress

## Specific ask for the next agent
Targeted technical question or validation request.
```

## Local Development & Defaults

- Read API keys (`OPENAI_API_KEY`, `GROQ_API_KEY`) only from environment variables at runtime.
- The default configuration path is `~/Library/Application Support/MICE/config.toml`; never add a real config file to git.
- Automated tests must run completely network-free using mock servers and deterministic fixtures.
