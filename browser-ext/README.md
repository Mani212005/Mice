# MICE Browser Guide extension

Load this directory as an unpacked Manifest V3 extension in a Chromium browser.
Start the Rust bridge with a runtime-only token:

```sh
MICE_BROWSER_BRIDGE_TOKEN='choose-a-long-random-token' mice browser-bridge
```

Open the extension's Options page, enter the same token, then use its popup to
ask a guide-me question. The token stays in browser extension storage and is
never written to this repository.

It exposes two messages for the local browser bridge:

- `mice.guide.snapshot` returns visible interactive DOM elements.
- `mice.guide.highlight` scrolls to and outlines a returned selector.

The bridge uses OpenAI `gpt-5.6-sol` with a strict output schema by default. If
MICE's configured cloud model is a Groq model such as
`llama-3.3-70b-versatile`, it uses Groq's JSON Object Mode instead. Both paths
return a candidate ID rather than a free-form CSS selector. The bridge resolves
that ID to the original supplied selector before asking the extension to scroll
to and outline it. Snapshot selectors are unique:
stable IDs and test IDs are used when available, with a structural CSS path as
the fallback, so repeated controls such as several `button` elements cannot be
confused. The extension first ranks up to 500 visible interactive elements
against the guide question (including labels, placeholders, and text-input
roles), then emits its best 100 with capped labels. The bridge independently
sanitizes, ranks, deduplicates, and limits the model prompt to its best 80
candidates. Each request highlights one target. Native global-screen overlay
boxes use the separate `mice-ipc` screen-coordinate contract.
