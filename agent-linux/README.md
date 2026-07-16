# MICE Linux agent scaffold

This package is the Linux-side placeholder for MICE's existing stdio
length-prefixed JSON-RPC protocol. It sends the same `initialize` handshake as
the macOS agent and truthfully advertises no native capabilities yet.

Future Linux work belongs here:

- PipeWire and xdg-desktop-portal screen capture
- AT-SPI accessibility lookup
- libei input monitoring/injection where supported
- a Linux-native overlay and clipboard adapter

Run the scaffold with:

```sh
cargo run -p mice-linux-agent
```
