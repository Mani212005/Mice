# MICE Autopilot Architecture Specification

This document provides a precise, comprehensive technical specification of **MICE Autopilot**. It covers the end-to-end architecture, execution pipeline, state machine, embedding models, JSON schema constraints, safety verifiers, recipe store, site knowledge base, and generation-scoped UID resolution.

---

## 1. System Overview

MICE Autopilot is an autonomous browser agent that executes high-level web goals (e.g., `mice autopilot "go to en.wikipedia.org and search for the James Webb Space Telescope"`).

### Core Design Principles:
1. **CDP Accessibility Bridge**: Interacts with Chrome via `chrome-devtools-axi` over the Chrome DevTools Protocol (CDP).
2. **Local Privacy Default**: Executes primarily using local LLMs (`gemma3:4b` default, `phi4-mini` text alternative) via Ollama, with opt-in cloud fallback.
3. **Generation-Scoped UID Resolution**: Re-resolves element targets dynamically across DOM generation bumps instead of relying on brittle CSS selectors or raw cross-session UIDs.
4. **Verifier-Gated Completion (M18c)**: Validates completion against live page identity (`RootWebArea` title & URL) rather than trusting self-reported model output.
5. **Safety-First Mutation Control**: Refuses sensitive fields (credentials, payment data, OTPs) and requires explicit batch confirmation for mutating browser actions.

---

## 2. System Architecture & Component Diagram

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                                 MICE CLI                                        │
│  mice autopilot "go to <site> and <action>"                                     │
└────────────────────────┬────────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                           AxiSessionGuard & Environment                         │
│  - Profile Isolation: CHROME_DEVTOOLS_AXI_USER_DATA_DIR                         │
│  - Headed Mode Default: CHROME_DEVTOOLS_AXI_HEADED=1                            │
└────────────────────────┬────────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                        Observation & Re-resolution Loop                         │
│  - Bridge: observe_axi() -> `npx chrome-devtools-axi snapshot`                  │
│  - Snapshot Budget: AXI_OBSERVATION_TOKENS = 2,000 tokens (~8,000 chars)       │
│  - Ordinal Matching: identity_of() -> (role, accessible_name, occurrence_index) │
└────────────────────────┬────────────────────────────────────────────────────────┘
                         │
         ┌───────────────┴────────────────────────┐
         │                                        │
         ▼                                        ▼
┌────────────────────────────────┐       ┌────────────────────────────────┐
│   Recipe Store (Cosine > 0.85) │       │   Site Knowledge Store (M19a)  │
│ - nomic-embed-text:latest      │       │ - nomic-embed-text:latest      │
│ - ~/Library/Application        │       │ - ~/Library/Application        │
│   Support/MICE/recipes/        │       │   Support/MICE/knowledge/      │
└────────────────┬───────────────┘       └────────────────┬───────────────┘
                 │                                        │
                 └──────────────────┬─────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                        Decision Engine (call_axi_agent_turn)                    │
│  - Two-Branch JSON Schema: anyOf (click/fill requires candidate_id string)      │
│  - Stream Timeout: OLLAMA_STREAM_IDLE_TIMEOUT = 90s                            │
│  - Budget: AXI_FRESH_DECISION_LIMIT = 12, AXI_MAX_CONSECUTIVE_REPLANS = 4       │
└────────────────────────┬────────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                        Safety & Verification Pipeline                           │
│  - blocked_browser_action(): Refuses sensitive fields & non-interactive roles   │
│  - controls_within(): Indentation parser for landmark container controls       │
│  - verify_goal_completion(): Option 1 (RootWebArea/URL) + Option 3 (Interaction)│
└────────────────────────┬────────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                     Verified Action Execution & Settling                        │
│  - execute_verified_browser_action() -> browser.open / click / fill             │
│  - Post-Action Settling Delay: std::thread::sleep(1000ms) after mutating step   │
└─────────────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Core Engine Components

### 3.1. Process & Session Management (`AxiSessionGuard`)
- **Profile Directory**: Auto-configures an isolated persistent Chrome profile at `~/Library/Application Support/MICE/chrome-profile` (macOS) or `$XDG_DATA_HOME/MICE/chrome-profile` (Linux) to reduce CAPTCHA triggers across runs.
- **Headed Display**: Sets `CHROME_DEVTOOLS_AXI_HEADED=1` by default so browser interactions are visible to the user during confirmation prompts.
- **Process Lifecycle Guard**: Implements `Drop` for `AxiSessionGuard` to clean up child processes on termination. Detects locked profiles and provides immediate diagnostics (`npx chrome-devtools-axi stop`).

### 3.2. Snapshot & Generation-Scoped UIDs
`chrome-devtools-axi` assigns UIDs prefixed with a generation counter (e.g., `g5:3_7`, `g9:6_12`, `g14:10_9`). Any DOM mutation or navigation bumps the generation counter.
- **Newtype Invariant Protection**:
  - `FreshSnapshot<'a>(pub &'a BrowserSnapshot)`
  - `ModelSnapshot<'a>(pub &'a BrowserSnapshot)`
  - Parameter inversion is prevented at compile time.
- **Ordinal Identity Matching**:
  - Elements are identified by the 3-tuple `(role, accessible_name, occurrence_index)`.
  - When matching dynamic pages with duplicate element labels (e.g. multiple "Submit" buttons), `target_order` tracks DOM insertion sequence so the $n$-th occurrence in a prior turn maps deterministically to the $n$-th occurrence in the fresh snapshot.

---

## 4. Machine Learning & Embedding Specifications

### 4.1. Text Embedding Model
- **Model**: `nomic-embed-text:latest`
- **Endpoint**: Ollama Embed API (`http://127.0.0.1:11434/api/embed`)
- **Embedding Dimensions**: 768-dimensional dense vector.
- **Usage**:
  - Keying and retrieving recipes by cosine similarity matching.
  - Domain and topic pre-filtering for site knowledge snippets.

### 4.2. Recipe Matching Mechanics (M19a / M19b)
- **Cosine Similarity Threshold**: `similarity > 0.85`
- **Storage Location**: `~/Library/Application Support/MICE/recipes/recipe-<timestamp>.json`
- **Recipe Data Structure**:
  ```json
  {
    "recipe_id": "recipe-1785047502",
    "goal_pattern": "go to en.wikipedia.org and search for the James Webb Space Telescope",
    "goal_embedding": [ ... 768 float values ... ],
    "steps": [
      {
        "call": { "name": "browser.open", "args": { "url": "https://en.wikipedia.org/" } },
        "target_role": null,
        "target_context": null
      },
      {
        "call": { "name": "browser.fill", "args": { "uid": "g8:6_12", "text": "James Webb Space Telescope" } },
        "target_role": "searchbox",
        "target_context": "Search Wikipedia"
      },
      {
        "call": { "name": "browser.click", "args": { "uid": "g11:9_9" } },
        "target_role": "button",
        "target_context": "Search"
      }
    ],
    "negative_constraints": []
  }
  ```
- **Persistence Quality Filter (`recipe_is_worth_saving`)**:
  Refuses to persist navigation-only sequences (`browser.open` only). Only sequences containing at least one real interaction (`fill`, `click`) are saved.

### 4.3. Site Knowledge Base (M19a)
- **Storage Location**: `~/Library/Application Support/MICE/knowledge/knowledge-<hash>.json`
- **Retrieval Threshold**: Cosine similarity `> 0.65` after domain pre-filter matching.
- **Instruction Injection**: Injected dynamically into system prompts under `Known facts about this site that may help:`.
- **CLI Commands**:
  - `mice knowledge add <site> "<fact>"`
  - `mice knowledge list`

---

## 5. Structured Decision Schema & LLM Constrained Decoding

To ensure deterministic JSON outputs from local LLMs (`gemma3:4b`), `call_axi_agent_turn` uses a two-branch `anyOf` JSON schema.

### 5.1. Two-Branch JSON Schema (`axi_decision_schema`)
```json
{
  "type": "object",
  "properties": {
    "say_to_user": { "type": "string" },
    "action": { 
      "type": "string", 
      "enum": ["click", "fill", "open_url", "scroll", "done", "handoff", "ask_user"] 
    },
    "candidate_id": { "type": ["string", "null"] },
    "url": { "type": ["string", "null"] },
    "value": { "type": ["string", "null"] },
    "done_summary": { "type": ["string", "null"] },
    "question": { "type": ["string", "null"] },
    "extracted_data": { "type": ["object", "string", "null"] }
  },
  "required": ["say_to_user", "action"],
  "additionalProperties": false,
  "anyOf": [
    {
      "properties": {
        "action": { "enum": ["click", "fill"] },
        "candidate_id": { "type": "string" }
      },
      "required": ["action", "candidate_id"]
    },
    {
      "properties": {
        "action": { "enum": ["open_url", "scroll", "done", "handoff", "ask_user"] }
      },
      "required": ["action"]
    }
  ]
}
```

### 5.2. Schema Invariants:
1. **Target Actions (`click`, `fill`)**: `candidate_id` MUST be a non-null string containing a valid UID token from the snapshot.
2. **Non-Target Actions (`open_url`, `scroll`, `done`, `handoff`, `ask_user`)**: `candidate_id` may be `null`.
3. **Stream Idle Timeout**: `OLLAMA_STREAM_IDLE_TIMEOUT = 90s` (resets per token; prevents wedged daemon hangs).

---

## 6. Safety Guards & Action Refusals

### 6.1. Sensitive Target Refusal (`blocked_browser_action`)
MICE automatically blocks interaction with sensitive fields:
- **Sensitive Fill Terms**: `password`, `passcode`, `one-time`, `otp`, `cvv`, `cvc`, `card number`, `credit card`, `debit card`, `routing number`, `account number`.
- **Sensitive Control Buttons**: `sign in`, `log in`, `submit payment`, `transfer`, `purchase`.

### 6.2. Non-Interactive Role Blocking & Indentation Discovery
- **Blocked Non-Interactive Roles**: `landmark`, `region`, `group`, `search`, `banner`, `navigation`.
- **Landmark Container Resolution (`controls_within`)**:
  When a model attempts to click a container landmark (e.g. `search` landmark), MICE refuses the landmark click and parses accessibility snapshot indentation to name the exact interactive controls inside it:
  > *"That decision was rejected: target 'search' is a container landmark. Choose one of the interactive controls inside it instead: searchbox 'Search Wikipedia', button 'Search'."*

### 6.3. Immediate Refusal Veto
When a proposed action is refused by safety checks or non-interactive role guards, it is **immediately** added to `negative_constraints` as an active rule, preventing the model from wasting turns re-proposing the same target.

---

## 7. Completion Verification Pipeline (M18c)

`AgentAction::Done` proposals are verified by `verify_goal_completion(...)` before acceptance:

```
[Model emits AgentAction::Done or Post-Action Observation Arrives]
                         │
                         ▼
┌────────────────────────────────────────────────────────┐
│ Option 3 Check: Minimum Interaction Requirement       │
│ - Interactive goal (search/fill/click) cannot finish  │
│   with only browser.open (navigation-only).           │
└────────────────────────┬───────────────────────────────┘
                         │ Pass
                         ▼
┌────────────────────────────────────────────────────────┐
│ Option 1 Check: Page Identity Keyword Corroboration    │
│ - Extracts subject keywords from goal (excluding stop  │
│   words & domain tokens).                              │
│ - Corroborates keywords against RootWebArea title & URL│
│   (ignoring editable input value text).                │
└────────────────────────┬───────────────────────────────┘
                         │ Pass
                         ▼
             [Goal Certified Complete]
```

### Verification Evaluation Rules:
1. **Auto-Consultation**: Evaluated after **every observation** once interaction starts, completing immediately upon reaching the target page.
2. **Identity-Only Scope**: Keyword corroboration checks `RootWebArea` title and page URL only. Snapshot body text (which contains MICE's typed input text and autocomplete dropdowns) is excluded to prevent forged completion claims.
3. **Threshold**: Requires $\ge \lceil N / 2 \rceil$ keyword matches on pages with $> 2$ subject keywords.

---

## 8. System Operating Limits & Constants

| Constant | Value | Description |
|---|---|---|
| `AXI_FRESH_DECISION_LIMIT` | `12` | Maximum fresh decision turns per session |
| `AXI_REPLAY_ACTION_LIMIT` | `200` | Maximum steps per recipe replay |
| `AXI_MAX_CONSECUTIVE_REPLANS` | `4` | Maximum consecutive soft-refusal replans |
| `AXI_LOCAL_UNCERTAINTY_LIMIT` | `2` | Uncertainty threshold before cloud fallback prompt |
| `AXI_OBSERVATION_TOKENS` | `2,000` | Snapshot prompt token budget (~8,000 characters) |
| `AXI_HISTORY_WINDOW` | `8` | Recent history items retained before compaction |
| `AXI_MAX_FILL_VALUE_CHARS` | `500` | Maximum character length for `fill` value strings |
| `OLLAMA_STREAM_IDLE_TIMEOUT` | `90s` | Idle read timeout for Ollama API streaming |
| `AXI_RECIPE_EMBEDDING_MODEL` | `nomic-embed-text:latest` | Vector embedding model for recipes and knowledge |

---

## 9. Verification & Maintenance Commands

```bash
# 1. Clear leaked Chrome profiles before testing
pkill -9 -f "MICE/chrome-profile"

# 2. Run full workspace test suite (164+ tests)
cargo test --workspace

# 3. Enforce zero Clippy warnings
cargo clippy --workspace --all-targets -- -D warnings

# 4. Enforce clean code formatting
cargo fmt --check

# 5. Execute Autopilot task live
./target/debug/mice autopilot "go to en.wikipedia.org and search for the James Webb Space Telescope"
```
