# MICE Browser Companion

Load this directory as an unpacked Manifest V3 extension once, then install
the native messaging host once after building MICE:

```sh
cargo run -p mice-cli -- setup-browser
```

There is no popup, token, options page, local port, or browser configuration.
When MICE is running, Chrome launches the native host automatically. The action
badge is only a connected/disconnected indicator.

For a Goal Guide step hinted as Chrome, Safari, Firefox, Browser, or website,
the core pushes that one instruction over Chrome native messaging. The
extension returns a fresh bounded snapshot, the existing candidate-ID flow
selects a verified target, and the extension highlights it.

Web Autopilot is started from the terminal:

```sh
cargo run -p mice-cli -- start
# In a second terminal:
cargo run -p mice-cli -- autopilot "search Canva and open a portrait"
```

`mice start` is the resident daemon and owns the browser companion socket;
leave it running while autopilot is in use. After one goal-level confirmation,
autopilot observes the page again after every
action and can click, fill non-sensitive fields, open an HTTP(S) URL, or
scroll—only using verified candidates. It always hands passwords, one-time
codes, payment data, logins, transfers, purchases, and final submissions back
to the user. The first successfully completed run asks before each safe action,
then future runs use goal-level consent. Turn on `careful_mode` in `mice
settings` to keep per-action confirmation permanently. Press Esc to stop an
active run.
