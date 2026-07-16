# MICE — Smart Cursor · Technical Plan v2

> **MICE** (working name — plural of *mouse*): an AI layer on top of the OS, driven from the cursor.
> **Target:** macOS first (Apple M4), Linux (Wayland) second.
> **Delivery (now):** terminal-first — a `mice` CLI. Downloadable `.app` deferred.
> **Provider:** OpenAI-only (OpenAI hackathon). Multi-provider later.
> **Implementer:** OpenAI **Codex CLI** (autonomous agent). This doc is written to be self-contained for it.
> **Changed since v1:** delivery model (terminal-first), real July-2026 OpenAI model IDs, M4/16 GB/29.3 GB-disk reality, terminal-inherited TCC strategy, Codex-CLI build sequencing.

---

## 0. Locked decisions (the spine)

1. **Process-split architecture.** Rust portable core ↔ thin Swift native agent, over local IPC — *not* FFI. Mirrors the MCP-server / Bolo pipeline pattern already built.
2. **Terminal-first delivery.** Entry point is a `mice` CLI (Rust). `mice settings` opens a ratatui **TUI**; `mice start` runs the tool. On-screen overlays stay native GUI (they must). `.app` packaging, signing, notarization → deferred.
3. **Async-first.** Overlay + spinner paint in <50 ms. AI response (1–8 s) streams in independently. UI never blocks on the model.
4. **Dual-capture.** Every selection/hover yields *both* a pixel crop (PNG) *and* an AX-tree snapshot when available. Both feed the router.
5. **Two-lane AI, real IDs.** Cloud default `gpt-5.6-luna`/`terra`/`sol` (vision + reasoning) and `gpt-image-2` (images); local `gpt-oss-20b` (Ollama/MLX) as privacy-mode + offline fallback. All OpenAI-family.
6. **Graceful degradation.** Features prefer the cheap path and fall back, never break.
7. **Scope discipline.** "Guide me" is **browser-scoped** (extension + DOM). Arbitrary-native-app guidance is out.

---

## 1. Delivery model — terminal-first now, `.app` later

**Now (hackathon):**
- One CLI binary: **`mice`** (Rust). It *is* the core daemon's front door.
- `mice start` — launches core + spawns the Swift agent; runs resident.
- `mice settings` — opens a **ratatui TUI** (reuse Bolo experience) to edit gesture bindings, model prefs, privacy mode. "The settings/UI appears" = this TUI.
- `mice status` — permission + feature + IPC-connection status.
- `mice stop` — shuts down.

**Critical distinction for the implementer:** the *control surface* is terminal (CLI + TUI). The *product surface* — the floating prompt beside the cursor and the guide-me highlight boxes — is **on-screen GUI drawn by the Swift agent** (AppKit windows), because a cursor tool paints anywhere on screen. Do **not** try to render cursor overlays in the terminal. Terminal launches and configures; the agent draws.

**Deferred (post-hackathon):** `.app` bundle, menu-bar item, DMG/installer, Developer-ID signing + hardened runtime + notarization. See §12 for why terminal-first *simplifies* permissions in the meantime.

---

## 2. Scope

**In (v1 product):** region select → content-type detect → floating prompt → action → stream result → replace/copy/side-panel · action presets (explain, summarize, rewrite, translate, extract, →JSON, code, image/infographic, Q&A) · multi-representation clipboard · hover intelligence (AX-preferred, vision-fallback) · browser guide-me (extension + structured highlights) · configurable gestures · local+cloud routing with a privacy (local-only) mode.

**Out (deferred):** guide-me across arbitrary native apps · full plugin SDK (extract later, don't design upfront) · video frame analysis · Windows (Linux is port #2).

---

## 3. Core architecture — process-split, not FFI

```
                         ┌──────────────────────────────────────────────┐
                         │        PLATFORM AGENT  (Swift, mac-native)     │
   OS surfaces  ◀──────▶ │  screen capture (ScreenCaptureKit) · input     │
                         │  hooks (CGEventTap) · text inject (AX/CGEvent) │
                         │  AX read (AX API) · OCR (Vision.framework)     │
                         │  overlay windows (AppKit)                      │
                         └───────────────────────┬────────────────────────┘
                                                 │  local IPC
                                                 │  (unix socket / stdio,
                                                 │   length-prefixed JSON-RPC 2.0)
                         ┌───────────────────────┴────────────────────────┐
                         │     CORE / `mice` CLI  (Rust, tokio)            │
                         │  CLI + TUI (ratatui) · gesture FSM · prompt     │
                         │  engine · provider router · clipboard engine · │
                         │  session cache · config · IPC server           │
                         └───────────────────────┬────────────────────────┘
                                                 │  HTTP / process
                         ┌───────────────────────┴────────────────────────┐
                         │                  AI LAYER                        │
                         │  LOCAL:  gpt-oss-20b   (Ollama or MLX)           │
                         │  CLOUD:  OpenAI Responses API                    │
                         │          gpt-5.6-luna / terra / sol  (vision)    │
                         │          gpt-image-2  (Images API)               │
                         │          gpt-realtime-mini  (optional voice)     │
                         └──────────────────────────────────────────────────┘
```

**Why process-split, not FFI**
- Swift owns the surfaces it's unbeatable at (ScreenCaptureKit, Vision, AX, CGEventTap, overlay). No objc2/core-foundation binding hell.
- Rust owns the portable brain + the `mice` CLI/TUI. The part that survives the Linux port untouched.
- **Linux port = swap the agent.** Replace the Swift agent with a Rust/Wayland agent (PipeWire + xdg-desktop-portal + libei). Core + AI layer don't move a line.
- Crash isolation + native async runtimes on each side (tokio / Swift concurrency), no bridging.

**Cost paid:** an IPC protocol + two build systems — a boundary already built once (MCP: stdio → SSE → streamable HTTP).

---

## 4. Component map — what lives where

| Concern | Lives in | Notes |
|---|---|---|
| Raw event capture (mouse/key) | Swift agent | Only the native hook sees these. |
| **Gesture *interpretation*** | **Rust core** | Portable FSM; timing/chord/stroke logic. |
| Screen capture | Swift agent | ScreenCaptureKit; Screen Recording perm. |
| Local OCR | Swift agent | Vision.framework — no Tesseract. |
| AX tree read | Swift agent | AX API; feeds hover + guide. |
| Text injection | Swift agent | AX set-value; CGEvent keystrokes fallback. |
| Overlay render | Swift agent | **Dumb** — renders what core commands. |
| CLI + TUI settings | Rust core | `mice` binary; ratatui. |
| Prompt templating | Rust core | Per-action templates. |
| Provider routing | Rust core | Capability match; local vs cloud. |
| Clipboard format negotiation | Rust core | Multi-representation payload. |
| Cache / config | Rust core | Artifact-hash cache; config store. |
| AI calls | Rust core | Responses/Images API (HTTP); Ollama/MLX (local). |

Capture rule: every selection/hover yields `{ pixels: PNG, ax: AXSnapshot?, text?, dom? }`. Router decides which to use.

---

## 5. AI routing — real July-2026 model IDs

Integrate via the **Responses API** (current recommended surface; not legacy Chat Completions) for text/vision/reasoning/function-calling/structured-outputs, and the **Images API** for `gpt-image-2`.

**Cloud lane — OpenAI (all GPT-5.6 models are multimodal / vision-capable):**

| Model ID | Role in MICE |
|---|---|
| `gpt-5.6-luna` | Cost-sensitive, high-volume: meta-routing classification, quick vision (hover fallback, "explain this chart"), short transforms. **Demo default for the fast path.** |
| `gpt-5.6-terra` | Balance tier: mid-weight reasoning + vision when Luna underperforms. |
| `gpt-5.6-sol` (alias `gpt-5.6`) | Frontier: heavy reasoning, guide-me structured output, hard vision. Supports functions + computer-use tools. |
| `gpt-image-2` | **The money shot** — select table/chart → "make an infographic" → rendered image. Reasoning-powered, near-perfect text rendering, Instant (default) + Thinking modes. Images API; ~$0.02–0.19/image by quality. |
| `gpt-realtime-mini` | Optional voice-driven guidance. |

> `gpt-image-2` is the current SOTA image model (Apr 2026), notable for accurate in-image text — exactly the old infographic failure mode, now solved. `gpt-image-1.5` / `gpt-image-1` / `gpt-image-1-mini` remain available as cheaper fallbacks. Org verification may be required to call GPT-Image models.

**Local lane — `gpt-oss-20b`** (OpenAI open weights, Apache 2.0). Runs via **Ollama** (simplest) or **MLX** (fastest on Apple Silicon). Used for: content-type classification, hover explanation when AX is rich, short rewrites/summaries/translations of extracted text, OCR cleanup, and privacy/offline mode. **Not the demo default** — see §8.

Routing decisions themselves can be made by the local model (cheap) or `gpt-5.6-luna` (§11).

---

## 6. Hard features via graceful degradation

### 6.1 Hover intelligence — "what does this do?"

**Prefer the AX tree, never depend on it.**
1. On hover-hold, ask AX API for the element → `{ role, title, value, help, actions }`.
2. **AX rich** (native apps): explain locally with `gpt-oss-20b` (or `gpt-5.6-luna`).
3. **AX sparse/absent** (Electron, custom-drawn, games): fall back to a **cloud vision** call (`gpt-5.6-luna`) on the pixel crop you already captured.

AX-preferred, vision-fallback → degrades in cost, not availability. macOS AX is the most coherent of the three OS trees, so mac-first makes this the easiest platform to land it on.

Output → overlay panel:
```json
{ "element":"…", "purpose":"…", "when_to_use":"…", "consequences":"…", "next_step":"…" }
```

### 6.2 Browser "guide me" — "where's Settings? highlight it"

Companion **browser extension** provides a clean DOM. Flow:
1. Extension serializes visible interactive elements (+ optional screenshot).
2. `gpt-5.6-sol` with **structured outputs** returns:
   ```json
   { "target_selector":"…", "bounding_box":[x,y,w,h], "instruction_text":"…", "next_step":"…" }
   ```
3. Overlay draws a highlight box; extension can scroll-to / outline.

Schema-constrained → precise, reliable. Browser-scoped on purpose.

---

## 7. Smart clipboard — the trick that saves weeks

Don't "detect target app and convert" for common cases. Every OS clipboard holds **multiple simultaneous representations**. Write `plain-text + RTF + HTML + PNG` at once; the destination picks the richest it understands (Excel→table, Word→formatted, Markdown editor→MD, IDE→code, Notion→rich blocks). Free, via OS negotiation. Real app-detection only for bespoke targets (native Excel grid, Notion block API) — later polish.

Rust core builds the payload; Swift agent writes all types to `NSPasteboard`.

---

## 8. Hardware reality — Apple M4 · 16 GB RAM (assumed) · 29.3 GB free disk

**Assumption:** base M4 = 16 GB unified memory. *Confirm exact RAM — if 24 GB+, the local lane becomes a comfortable default instead of a fallback.*

**RAM:** `gpt-oss-20b` is designed for ~16 GB systems and is MoE (~3.6 B active params), so on M4's memory bandwidth throughput is fine. But headroom on 16 GB is thin alongside OS + agent + terminal — expect occasional swap. **Mitigation:** lazy-load the local model, unload when idle, keep it for genuinely small jobs.

**Disk is the real pinch (29.3 GB free):**
- `gpt-oss-20b` weights ≈ **13 GB** (MXFP4/4-bit).
- **Use Command Line Tools, not full Xcode** (~10 GB saved). Build the Swift agent with `swift build` + AppKit programmatically; no `.xcodeproj` needed for a headless-ish agent.
- Watch cargo/target and node_modules growth; add a `mice doctor` disk check later.

**Recommendation:** **cloud-first for the demo** (`gpt-5.6-luna` fast path), **local (`gpt-oss-20b`) as the privacy-mode toggle + offline fallback** — not the default hot path. Smoother demo, and it still tells the "OpenAI open weights on-device" story.

---

# DEEP DIVES

---

## 9. Deep dive — IPC protocol (Rust core ↔ Swift agent)

### 9.1 Process model
- **`mice` CLI (Rust) is launched from the terminal** and is the parent. It runs the core and **spawns the Swift agent** as a child.
- Transport: **stdio pipes** to the agent for v1 (simplest, dies with parent); abstract behind a `Transport` trait so a **unix domain socket** (`~/Library/Application Support/MICE/agent.sock`) can replace it if you want independent restart.
- Launching the agent as a child of the terminal-run CLI matters for TCC inheritance (§12).

### 9.2 Framing — length-prefixed
```
[4-byte little-endian u32 length][UTF-8 JSON payload]
```
Robust for base64 PNGs and interleaved streaming chunks (NDJSON breaks on multiline).

### 9.3 Protocol — JSON-RPC 2.0 (MCP-shaped)
Request (`id`+`method`+`params`) · Response (`id`+`result`|`error`) · Notification (`method`+`params`, no `id`; used for events and streaming chunks).

### 9.4 Handshake — capability negotiation (keeps Linux port clean)
```json
// agent → core
{ "jsonrpc":"2.0","id":1,"method":"initialize",
  "params":{ "protocolVersion":"1.0","platform":"macos",
    "capabilities":{ "screen_capture":true,"ax_read":true,"inject_text":true,
      "overlay":true,"local_ocr":true,"browser_bridge":false } } }
```
The **router reads these caps** to know what's possible on this platform. A future Linux agent advertises different caps; core code unchanged.

### 9.5 Method surface
**Agent → Core (notifications):** `gesture.triggered` · `selection.captured{sessionId,pixels:b64,ax?,text?,bounds}` · `hover.entered` · `prompt.submitted{sessionId,instruction,action?}` · `prompt.cancelled`.
**Core → Agent (commands):** `overlay.show` · `overlay.appendResult{sessionId,chunk}` (streaming) · `overlay.finishResult{actions}` · `overlay.highlight{boxes}` (guide) · `overlay.dismiss` · `clipboard.set{reps}` (request) · `text.inject{target,content,mode}` (request).

### 9.6 Streaming + cancel
AI output streams as `overlay.appendResult` notifications keyed by `sessionId`, terminated by `overlay.finishResult`. `prompt.cancelled` aborts the in-flight AI future and drops the stream.

### 9.7 Reliability
`ping`/`pong` heartbeat → missed → respawn agent + replay `initialize`. Bounded channel + chunk coalescing for backpressure. Error enum: `AI_FAILED` · `LOCAL_OOM` · `PERMISSION_DENIED` · `CANCELLED`.

---

## 10. Deep dive — gesture state machine (Rust core, portable)

### 10.1 Core problem
Distinguish chords, holds, multi-clicks, drags, strokes from **normal mouse use — without swallowing normal clicks.** `CGEventTap` decides *synchronously* to pass or consume each event → FSM must decide fast and default to pass-through.

### 10.2 Design rule: gate gestures behind a trigger
Never hijack bare left-click. Gestures live behind an explicit trigger:
- **Chord** — e.g. `Left+Right` within a chord window → discrete action (open prompt).
- **Hold** — a button/modifier held > threshold *without* movement → context action (explain-under-cursor).
- **Stroke mode** — hold a designated button (e.g. right) + move → record stroke; release → recognize + execute.

Proven model (StrokesPlus-style). Bare clicks always pass through; only confirmed gestures consume events.

### 10.3 States
```
IDLE ─trigger─▶ ARMED ─move>thresh─▶ CAPTURING(stroke|region) ─release─▶ RECOGNIZE ─▶ DISPATCH ─▶ IDLE
   ▲              │timeout                                                                 │
   └──────────────┴──────────────── Esc / timeout ──────────────────────────────────────┘
```
IDLE pass-through + watch triggers · ARMED start timers, buffer events (non-gesture → replay/pass) · CAPTURING record stroke points / drag bounds · RECOGNIZE classify · DISPATCH emit `gesture.triggered` + geometry · Esc/timeout → IDLE.

### 10.4 Tunable constants (config-driven, editable in `mice settings`)
| Constant | Default | Meaning |
|---|---|---|
| `chord_window_ms` | 120 | Max gap for two buttons = chord. |
| `hold_threshold_ms` | 350 | Button-down → "long hold". |
| `move_threshold_px` | 6 | Under this = tap, not drag. |
| `multi_click_ms` | 300 | Double/triple window. |
| `stroke_min_px` | 20 | Min path to register a stroke. |
| `stroke_button` | right | Button entering stroke mode. |
| `arm_timeout_ms` | 500 | ARMED → IDLE if unresolved. |

### 10.5 Stroke recognition
Encode path as an **8-direction sequence** (quantize segment angles, collapse runs → e.g. `["E","S"]` = L-shape). Match a small template table. Cheap, deterministic, no ML. Upgrade to a `$1` recognizer only on template collisions.

### 10.6 Spec gestures → rules
| Gesture | Class | Rule |
|---|---|---|
| Left+Right → open prompt | chord | both down within `chord_window_ms` |
| Long middle → explain under cursor | hold | middle down > `hold_threshold_ms`, move < `move_threshold_px` |
| Double middle → screenshot region | multi-tap | two middle taps within `multi_click_ms` |
| Stroke → summarize page | stroke | `stroke_button` held + template match |
| Stroke → OCR region | stroke | template match |
| Drag region → select | region | down-move-up in mode, bounds > `move_threshold_px` |

### 10.7 Pass-through discipline (critical — first-class test target)
Default = observe, don't consume. Consume/suppress **only** once a gesture is confirmed (chord's second button, or `stroke_min_px` reached). If ARMED resolves to normal click, release buffered events so the app sees a clean click. Get this wrong → tool feels broken.

---

## 11. Deep dive — router decision logic (Rust core)

### 11.1 Inputs
```rust
struct RouteRequest {
    artifacts: Artifacts,          // { pixels?, ax?, text?, dom? }
    instruction: String,
    action: Option<ActionPreset>,  // explain|summarize|translate|extract|to_json|code|image|guide|qa
    agent_caps: Capabilities,      // from initialize handshake
    config: RouteConfig,           // privacy_mode, model_prefs, cost_policy
}
```

### 11.2 Model registry (config, not code — provider-plugin ready)
```rust
struct ModelDescriptor { id:String, locality:Locality, vision:bool, image_gen:bool,
                         reasoning_tier:u8, speed:Speed, cost:Cost }
```
Seed (July 2026):
```
gpt-oss-20b       Local  vision=false image=false tier=1 speed=med  cost=free
gpt-5.6-luna      Cloud  vision=true  image=false tier=2 speed=fast cost=low
gpt-5.6-terra     Cloud  vision=true  image=false tier=3 speed=med  cost=med
gpt-5.6-sol       Cloud  vision=true  image=false tier=4 speed=slow cost=high
gpt-image-2       Cloud  vision=true  image=true  tier=—  speed=slow cost=med   (Images API)
gpt-realtime-mini Cloud  (voice)                                                 (optional)
```
Adding a provider later = descriptor + adapter. No router rewrite (seed of the future plugin SDK — extracted, not designed upfront).

### 11.3 Decision pipeline
```
1. CLASSIFY need — needs_vision? needs_image_gen? complexity?
   Do it cheaply: gpt-oss-20b (local) OR gpt-5.6-luna, structured output:
   { needs_vision, needs_image_gen, complexity, suggested_action }

2. PRIVACY GATE — if privacy_mode==LocalOnly:
     needs cloud-only cap (vision/image) → UserVisibleError (never silently go cloud)
     else force Local lane.

3. CAPABILITY FILTER — candidates = registry.filter(satisfies caps ∧ locality allowed)

4. PICK by cost_policy — cheapest_that_satisfies (default) | fastest | best_quality

5. EXECUTE (streaming) with FALLBACK:
     cloud fail/offline → degrade to Local (+ note)
     local OOM (16 GB!) → escalate to Cloud (+ note)
```

### 11.4 Concrete routing table
| Artifact | Instruction | Route |
|---|---|---|
| text | summarize / translate | `gpt-oss-20b` local (privacy) or `gpt-5.6-luna` (demo) |
| text | extract → JSON | local; `gpt-5.6-terra` if complex |
| image/screenshot | explain this chart | `gpt-5.6-luna` vision |
| table+text | make an infographic | **`gpt-image-2`** |
| AX element (hover) | explain element | local if AX rich; `gpt-5.6-luna` vision if sparse |
| DOM+screenshot | where's Settings? | `gpt-5.6-sol` + structured output |
| code | explain / refactor | `gpt-5.6-terra`/`sol` (local for trivial) |

### 11.5 Caching
Key = `hash(artifact_bytes) + action + instruction`. Same selection + new instruction → reuse capture, re-route (instant "now translate it"). Same selection + same instruction → cache hit, no AI call.

### 11.6 Streaming + cancel
Router returns a chunk stream → forwarded as `overlay.appendResult`. `prompt.cancelled` aborts the future.

---

## 12. Deep dive — macOS permissions (terminal-first strategy)

### 12.1 Permissions needed (TCC)
| Permission | Enables | Dead if denied |
|---|---|---|
| **Screen Recording** | ScreenCaptureKit capture, vision, OCR | capture / vision / OCR / hover-fallback |
| **Accessibility** | AX read (hover/guide), AX text injection | AX hover, inject |
| **Input Monitoring** | `CGEventTap` global input listening | all gestures/shortcuts |

> Modern macOS gates `CGEventTap` listening behind **Input Monitoring**, *separate* from **Accessibility**. You need **both**, plus Screen Recording.

### 12.2 Terminal-first advantage (dev)
Run `mice` from Terminal/iTerm → TCC attributes the responsible process to **the terminal app**, and child processes (core → agent) **inherit** the grant. So:
- **Grant Terminal.app (or iTerm) the three permissions once** → no per-rebuild re-granting.
- **Skip Developer-ID signing + notarization** during the hackathon.
- Caveat: attribution up the child chain can be finicky across macOS versions. Launch the agent as a child of the terminal-run CLI, and **verify actual state via API** (`mice status`, below), not by assumption. If inheritance doesn't hold, grant the built binary directly as fallback.
- **Later, the packaged `.app` needs its own grants + stable signature** — that's when §12.4's signing notes apply.

### 12.3 State-check APIs (drive `mice status` + first-run wizard)
| Permission | Check | Request |
|---|---|---|
| Screen Recording | `CGPreflightScreenCaptureAccess()` | `CGRequestScreenCaptureAccess()` |
| Accessibility | `AXIsProcessTrusted()` / `AXIsProcessTrustedWithOptions(prompt:true)` | prompt option |
| Input Monitoring | `IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)` | `IOHIDRequestAccess(...)` |

Deep links:
```
x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture
x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility
x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent
```

### 12.4 Packaging notes (deferred)
When you build the `.app`: TCC keys on the code signature, so ad-hoc/changing signatures reset grants → use a **stable Developer-ID identity**; add hardened runtime + **notarization**; App Sandbox is generally incompatible with global taps/broad AX → ship non-sandboxed (Developer-ID, not Mac App Store). Info.plist usage strings (e.g. `NSAppleEventsUsageDescription`) are **mandatory or the app crashes** on request.

### 12.5 Degradation matrix
| Denied | Still works | Dead |
|---|---|---|
| Screen Recording | text-selection actions (if text via AX) | vision, OCR, hover-fallback, region capture |
| Accessibility | gestures (if Input Monitoring ok), capture | AX hover, injection → clipboard-only |
| Input Monitoring | menu/CLI-triggered actions | all gestures/global shortcuts |

Never hard-fail — `mice status` surfaces the exact missing grant + fix link.

---

## 13. Build sequencing for Codex CLI

### 13.1 Assumptions (state-and-proceed)
- Stack: **Rust workspace** (`mice` CLI + core) + **Swift package** (mac agent, built with Command Line Tools, no full Xcode).
- macOS 14+, Apple M4, 16 GB RAM (assumed), ~29 GB free disk.
- `OPENAI_API_KEY` in env. OpenAI **Responses API** for text/vision/reasoning; **Images API** for `gpt-image-2`. Org may need verification for GPT-Image.
- **Ollama** (or MLX) installed with `gpt-oss-20b` pulled — but cloud is the demo default.
- Dev run **from Terminal** with TCC granted to Terminal.app (§12.2).
- Config at `~/Library/Application Support/MICE/config.toml`.
- Follow clean-code principles; each milestone must pass its acceptance check before the next.

### 13.2 Workspace layout
```
mice/
  Cargo.toml                 # workspace
  crates/
    mice-cli/                # `mice` binary: subcommands + ratatui TUI; spawns agent
    mice-core/               # daemon: router, prompt engine, gesture FSM, clipboard, cache, IPC server
    mice-ipc/                # shared IPC types + length-prefixed framing (serde)
    mice-providers/          # OpenAI (Responses/Images) + Ollama/MLX adapters
  agent-macos/               # Swift package: capture, AX, CGEventTap, Vision OCR, overlays, IPC client
  browser-ext/               # guide-me extension (M3)
  config/                    # default config + gesture bindings
```

### 13.3 Milestones (each with acceptance + verification)

**M0 — Prove the scary bits (isolated, no wiring)**
- `A0.1` region capture → PNG in <100 ms. *Verify:* standalone Swift `capture-test` writes `/tmp/cap.png`, logs ms.
- `A0.2` inject text into focused app. *Verify:* focus TextEdit, run `inject-test`, `"hello"` appears.
- `A0.3` `CGEventTap` sees global input (Input Monitoring granted). *Verify:* logs live mouse/key events.
- `A0.4` AX element under cursor. *Verify:* hover a Safari button → prints `role`/`title`.
- **Gate:** all four pass before M1.

**M1 — The spine (end-to-end, cloud-only)**
- `mice start` spawns agent+core; `initialize` handshake OK; `mice status` shows connected + caps + all TCC granted.
- Chord gesture → capture region → `gpt-5.6-luna` (vision) via Responses API → stream into overlay → copy to clipboard.
- **Acceptance:** select a paragraph, chord, ask "summarize" → streamed summary in overlay + on clipboard.
- *Verify:* a `curl` to Responses API with a test image returns text; then manual e2e.

**M2 — Actions + smart clipboard + local lane + TUI**
- Action presets (explain/summarize/translate/extract→JSON/code).
- Multi-representation clipboard (text+html+rtf+png on `NSPasteboard`).
- `gpt-oss-20b` (Ollama/MLX) wired; router picks local for cheap text ops; privacy_mode toggle.
- `mice settings` ratatui TUI: edit gesture bindings, model prefs, privacy mode → persists to `config.toml`.
- **Money shot:** select table → "make an infographic" → `gpt-image-2` → image in side panel + on clipboard.
- **Acceptance:** paste into Excel yields a table; infographic renders with correct text; local-only mode blocks vision with a clear message.

**M3 — Hover + browser guide-me**
- Hover intelligence: AX-preferred (`gpt-oss-20b`) / vision-fallback (`gpt-5.6-luna`).
- Browser extension + guide-me: `gpt-5.6-sol` structured output → `overlay.highlight` boxes.
- **Acceptance:** hover an unfamiliar button → correct explanation; "where's Settings?" highlights the right element in-page.

**M4 — Packaging + Linux prep (deferred)**
- Wrap as signed/notarized `.app` + menu-bar item.
- Linux agent stub (PipeWire/portal/libei) behind the same IPC handshake; core unchanged.

---

## 14. Trade-offs & risks

**Trade-offs:** IPC + two build systems (vs. FFI pain avoided) · browser-scoped guide-me · local lane is fallback not default on 16 GB · non-sandboxed app later (Developer-ID, not App Store).

**Risks (ranked):**
1. **Disk (29.3 GB free).** `gpt-oss-20b` ~13 GB + toolchains is tight. Mitigate: CLT-not-Xcode, cloud-first, disk watch.
2. **RAM headroom on 16 GB** for local lane. Mitigate: lazy-load/unload, local-for-small-only, cloud default.
3. **Permission attribution** via terminal inheritance can be finicky across macOS versions. Mitigate: verify via API in `mice status`; grant binary directly if inheritance fails.
4. **Gesture pass-through discipline.** Get it wrong → tool feels broken. First-class test target.
5. **GPT-Image org verification** may gate `gpt-image-2`. Check developer console early.
6. **Model IDs current as of July 2026** — re-check before final submission; Responses API is the integration surface.

---

## 15. Open questions / next steps

**Open questions:** exact M4 RAM (16 vs 24 GB+ changes local-lane default) · prompt-box UX (inline-only vs persistent side panel) · which action presets ship in M1 vs later.

**Next steps:** (1) hand this doc to Codex CLI and run M0. (2) confirm `gpt-image-2` org access. (3) optionally push to Notion as a PRD.

---

*Plan v2 — terminal-first delivery · real July-2026 OpenAI model IDs · M4 hardware-grounded · Codex-CLI build sequencing. Verify model IDs at build time.*
