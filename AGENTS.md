# MICE contributor guidance

## Delivery order

1. Keep M0 native capability probes independent and passing before wiring M1.
2. Keep the portable Rust core separate from the macOS Swift agent. The core owns
   routing and state; the agent owns macOS surfaces, permissions, capture, and
   overlays.
3. Do not persist credentials, captures, clipboard contents, model weights, or
   user configuration in this repository.

## Architecture boundaries

- The agent is a child of `mice start` and communicates only through the
  `mice-ipc` length-prefixed JSON-RPC 2.0 protocol.
- Add or change protocol types in `crates/mice-ipc`; do not duplicate wire types
  in the CLI or macOS agent.
- Global input defaults to pass-through. An event may be consumed only once a
  configured gesture has been confirmed.
- Rust never renders native overlays. Swift never chooses providers or routing.

## Verification

- Run `swift build` in `agent-macos` after Swift changes.
- Run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and `cargo test --workspace` after Rust changes.
- M0 requires manual macOS permission verification for Screen Recording,
  Accessibility, and Input Monitoring. Each probe must fail clearly when its
  permission is missing.
- Keep automated tests network-free. Provider tests must use mock HTTP servers.

## Local development

- Read `OPENAI_API_KEY` only from the environment at runtime.
- The default config path is `~/Library/Application Support/MICE/config.toml`;
  never add a real config file to git.
- `gemma3:4b` is the default local privacy model. `phi4-mini` is a supported
  smaller text-only alternative. `gpt-oss:20b` is an opt-in heavy model only:
  require the hardware preflight to pass before enabling or downloading it.
