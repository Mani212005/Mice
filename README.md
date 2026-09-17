# MICE - Terminal Sidekick Sub-Agent for Coding LLMs

**MICE** is a privacy-first, high-speed terminal/TUI sidekick sub-agent designed to pair with orchestrator LLMs (such as Gemini 3.7 Flash, Claude Code, Antigravity, and Cursor). 

Inspired by the **Devin SWE 2.0 sidekick architecture**, MICE offloads token-heavy, routine engineering sub-tasks:
- Sub-millisecond semantic document and repository file retrieval
- AST symbol search and declaration lookup
- Surgical code patching and verified unified diff generation
- Fast local test suite runs with noise filtering and failure extraction
- In-memory semantic knowledge graph traversal

Instead of consuming tens of thousands of tokens streaming massive raw files and compiler logs into the main orchestrator, MICE executes these routines locally and returns compact, verified diffs and structured summaries, slashing context window costs and latency.

---

## Key Features

- **⚡ Sub-Millisecond Semantic Document Finder (`mice-core::finder`):** Instant local inverted-index semantic search across codebases, receipts, invoices, tax documents, and system files.
- **🧠 In-Memory Semantic Knowledge Graph (`mice-core::knowledge_graph`):** Ultra-lightweight (< 15 MB RAM) 2-hop entity and relationship graph.
- **🐭 Sidekick Execution Engine (`mice-core::sidekick`):** Deterministic and local SLM tool loops executing delegated coding sub-tasks.
- **📊 Token Savings & Cost Metrics:** Tracks ingested vs returned tokens, context compression ratio, and estimated dollar savings.
- **🖥️ Ratatui TUI Dashboard (`mice tui`):** Rich terminal dashboard displaying paired orchestrators, live delegated sub-agent tasks, cumulative token savings, and an interactive diff inspector.
- **🔌 Model Context Protocol (MCP) Server (`mice mcp-server`):** Direct stdio MCP tools (`mice_sidekick_task`, `mice_semantic_find`, `mice_knowledge_query`, `mice_token_savings`) for Claude Code, Cursor, Codex, and Antigravity.

---

## Architecture

```
projects/Mice/
├── crates/
│   ├── mice-core/         # Sidekick engine, SemanticFinder, KnowledgeGraph, token metrics
│   ├── mice-cli/          # Ratatui TUI dashboard, CLI dispatcher, MCP server implementation
│   ├── mice-ipc/          # JSON-RPC 2.0 protocol types and framing
│   └── mice-providers/    # Local Ollama & cloud provider integrations
├── Cargo.toml             # Workspace definition
└── README.md
```

---

## CLI Usage

### Interactive TUI Dashboard
Launch the interactive Ratatui dashboard:
```bash
mice tui
# or simply
mice
```
- `[d]` Delegate sample AST search
- `[s]` Query Semantic Document Finder
- `[t]` Run fast local test suite
- `[p]` Preview & apply verified code patch
- `[c]` Clear task history
- `[q]` Quit dashboard

### CLI Delegation
Run delegated routines directly from scripts or sub-agent runners:
```bash
# Semantic document search
mice delegate --kind semantic --prompt "get my electricity bill"

# AST symbol lookup
mice delegate --kind ast --prompt "SidekickEngine" --file "crates/mice-core/src/sidekick.rs"

# Fast local test execution
mice delegate --kind test --test-cmd "cargo test -p mice-core --lib"

# Output structured JSON for machine consumption
mice delegate --kind semantic --prompt "Aadhaar Card" --json
```

### Model Context Protocol (MCP) Server
Register MICE with Claude Code, Codex, or Cursor:
```bash
# Start MCP server over stdio
mice mcp-server

# Connect automatically to available coding harnesses
mice connect all
```

---

## Development & Verification

All changes in the workspace are validated with standard Rust tooling:

```bash
# Format check
cargo fmt --check

# Strict Clippy lint check
cargo clippy --workspace --all-targets -- -D warnings

# Comprehensive automated test suite
cargo test --workspace
```

---

## License

This project is licensed under the Apache-2.0 License.
