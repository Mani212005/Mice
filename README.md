# MICE

**MICE** is an AI helper designed for people who are weak with technology. It is a screen-aware, native agent that helps users accomplish goals on their computer by either doing it for them or patiently walking them through the steps on their actual screen, in plain language.

## Architecture

MICE is built with a focus on cross-platform portability and native capabilities:

- **Portable Core (Rust):** The core engine, written in Rust, manages routing, state, provider communication, and logic.
- **Native Agents (macOS & Linux):** Platform-specific agents handle surfaces, permissions, screen capture, and native UI overlays (e.g., Swift on macOS).
- **Communication (`mice-ipc`):** The native agents communicate with the Rust core exclusively via the `mice-ipc` length-prefixed JSON-RPC 2.0 protocol.

### Multi-Agent Development

The repository is built iteratively by autonomous agents using a structured handoff protocol. To coordinate efforts between different AI workers on the same repository, a shared contract is maintained to ensure smooth handoffs.

## Features

- **Observe → Decide → Act Loop:** MICE dynamically navigates and interacts with the user's screen by observing current state, making decisions, and acting upon them, rather than relying on static plans.
- **Privacy First:** MICE prioritizes local computation. By default, it uses local, privacy-focused models like `gemma3:4b` or `phi4-mini`. Heavier opt-in models require specific hardware validation.
- **Strict Boundaries:** Credentials, captures, clipboard contents, model weights, and user configurations are explicitly excluded from persistence.

## Development

### Prerequisites

- macOS (for `agent-macos`) or Linux (for `agent-linux`).
- Rust (Cargo) for the core `crates/` workspace.
- Swift for building native macOS agents.

### Building & Verification

- **macOS Agent:** Run `swift build` in the `agent-macos/` directory.
- **Rust Core:** Run the following commands in the root directory:
  - Format check: `cargo fmt --check`
  - Linting: `cargo clippy --workspace --all-targets -- -D warnings`
  - Tests: `cargo test --workspace`

### Configuration

For local development:
- The default configuration path is `~/Library/Application Support/MICE/config.toml` (macOS). Never add a real configuration file to source control.
- Read API keys (e.g., `OPENAI_API_KEY`) only from environment variables at runtime.
- Automated tests must run completely network-free using mock HTTP servers.

## License

This project is licensed under the Apache-2.0 License.
