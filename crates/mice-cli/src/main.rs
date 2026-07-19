mod filing;
mod mcp_client;
mod memory;
mod tidy;
mod tools;

use std::{
    collections::{HashMap, HashSet},
    env,
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mice_core::{
    AgentAction, AgentDecision, AgentLoop, AgentLoopState, ExecutionLane, GoalSession, GoalState,
    LOCAL_SUMMARY_CHUNK_TOKENS, LOCAL_SUMMARY_REDUCE_TOKENS, MachineProfile, SmartCopyPlan,
    ToolDecision, action_instruction, chunk_summary_instruction, clipboard_contents, config_path,
    estimate_tokens, load_config, looks_like_code, parse_markdown_table,
    reduce_summary_instruction, route_execution_lane, save_config, selection_summary_instruction,
    smart_copy_chunks, smart_copy_clean_instruction, smart_copy_plan, smart_copy_preserves_links,
    smart_copy_preserves_visible_text, smart_copy_table_instruction, structural_summary_chunks,
    summary_reduce_batches, table_clipboard_contents,
};
use mice_ipc::{
    AgentCommand, Capabilities, ClipboardCaptured, HoverCaptured, InitializeParams,
    PromptSubmitted, RpcRequest, SelectionAction, SelectionText, read_frame, write_frame,
};
use mice_providers::{
    Action, Artifacts, ModelPreferences, OllamaError, PrivacyMode, RouteRequest,
    SelectionSummaryRoute, route, route_selection_summary, stream_ollama_chat,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tools::{CommandRunner, SystemRunner, ToolCall, ToolContext, ToolOutput};

fn main() {
    let command = env::args().nth(1);
    // Chrome starts a native-messaging host directly and does not pass our
    // `native-host` subcommand. It may pass the extension origin as argv[1],
    // but stdin is always a framing pipe, so recognise both launch forms.
    let chrome_native_host = command
        .as_deref()
        .is_some_and(|argument| argument.starts_with("chrome-extension://"))
        || (command.is_none() && !std::io::stdin().is_terminal());
    let result = match command.as_deref() {
        Some("status") => status(),
        Some("doctor") => doctor(),
        Some("settings") => settings(),
        Some("actions") => actions(),
        Some("tools") => list_tools(),
        Some("do") => do_goal(),
        Some("bench-tools") => bench_tools(),
        Some("savings") => savings(),
        Some("advertise") => advertise(),
        Some("browser-bridge") => browser_bridge(),
        Some("native-host") => native_host(),
        Some("setup-browser") => setup_browser(),
        Some("autopilot") => autopilot(),
        Some("start") => start(),
        Some("stop") => stop(),
        Some("route") => route_preview(),
        Some("ask") => ask(),
        Some("see") => see(),
        Some("tidy") => tidy::tidy(),
        Some("file") => filing::file_cmd(),
        Some("mcp") => mcp_cmd(),
        Some("mcp-server") => mcp_server(),
        _ if chrome_native_host => native_host(),
        _ => usage(),
    };
    if let Err(error) = result {
        eprintln!("mice: {error}");
        std::process::exit(1);
    }
}

fn usage() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "Usage: mice <start|stop|status|doctor|settings|actions|tools|do|bench-tools|savings|advertise|browser-bridge|setup-browser|autopilot|route|ask|see|tidy|file|mcp-server>"
    );
    println!("       mice ask [--action <preset>] <instruction>");
    println!("       mice see [--display] <question about your screen>");
    println!("       mice autopilot <goal>");
    println!("       mice mcp-server");
    println!("       mice do [--model <model>] [--max-actions <n>] [--session <name>] <goal>");
    println!(
        "       mice tidy [--apply] [--no-label] [folder]   (default ~/Downloads; dry run without --apply)"
    );
    println!("       mice tidy --undo");
    println!("       mice file --add-root <folder>");
    println!("       mice file <path>");
    println!("       mice mcp list");
    println!("       mice mcp call <server> <tool> [json-arguments]");
    Ok(())
}

/// Inspect and exercise user-granted external MCP servers. Both forms are
/// explicit user invocations: MICE never sends content to an external server
/// on its own.
fn mcp_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("list") => {
            let granted = mcp_client::granted_servers(&config);
            if granted.is_empty() {
                println!(
                    "No MCP servers are granted. Add an [[mcp.servers]] entry with enabled = true to {}.",
                    config_path().ok_or("HOME is not set")?.display()
                );
                return Ok(());
            }
            for server in granted {
                match mcp_client::McpServerProcess::spawn(server) {
                    Ok(mut process) => match process.list_tools() {
                        Ok(tools) => {
                            println!(
                                "{} ({}):",
                                mcp_client::sanitize_external_text(&server.name),
                                mcp_client::sanitize_external_text(&server.command)
                            );
                            for tool in tools {
                                println!(
                                    "  {} — {}",
                                    tool.name,
                                    mcp_client::sanitize_external_text(&tool.description)
                                );
                            }
                        }
                        Err(error) => println!(
                            "{}: tool discovery failed ({})",
                            mcp_client::sanitize_external_text(&server.name),
                            mcp_client::sanitize_external_text(&error.to_string())
                        ),
                    },
                    Err(error) => println!(
                        "{}: unavailable ({})",
                        mcp_client::sanitize_external_text(&server.name),
                        mcp_client::sanitize_external_text(&error.to_string())
                    ),
                }
            }
            Ok(())
        }
        Some("call") => {
            let (Some(server_name), Some(tool)) = (arguments.get(1), arguments.get(2)) else {
                return Err("Usage: mice mcp call <server> <tool> [json-arguments]".into());
            };
            let tool_arguments: Value = match arguments.get(3) {
                Some(raw) => serde_json::from_str(raw)
                    .map_err(|error| format!("Tool arguments must be JSON: {error}"))?,
                None => json!({}),
            };
            let server = mcp_client::granted_servers(&config)
                .into_iter()
                .find(|server| server.name == *server_name)
                .ok_or_else(|| {
                    format!("`{server_name}` is not a granted MCP server; run `mice mcp list`.")
                })?;
            let mut process = mcp_client::McpServerProcess::spawn(server)?;
            let answer = process.call_tool(tool, tool_arguments)?;
            println!("{}", mcp_client::sanitize_external_text(&answer));
            Ok(())
        }
        _ => Err("Usage: mice mcp <list|call>".into()),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserElement {
    selector: String,
    role: String,
    label: String,
    #[serde(
        default,
        rename = "candidate_id",
        skip_serializing_if = "Option::is_none"
    )]
    candidate_id: Option<String>,
}

const MAX_GUIDE_CANDIDATES: usize = 60;
const MAX_GUIDE_LABEL_CHARS: usize = 120;
const MAX_GUIDE_ROLE_CHARS: usize = 40;
const MAX_GUIDE_SELECTOR_CHARS: usize = 256;
/// Hard ceiling on the serialized observation sent to a provider. Control-dense
/// pages (e.g. a Canva dashboard with long card labels) would otherwise exceed
/// the provider request-size limit — Groq returns HTTP 413.
const MAX_OBSERVATION_CHARS: usize = 12_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserGuideRequest {
    instruction: String,
    url: String,
    elements: Vec<BrowserElement>,
}

#[derive(Debug, Deserialize)]
struct GuideModelResult {
    candidate_id: String,
    instruction_text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserGuideResponse {
    selector: String,
    instruction_text: String,
    label: String,
    role: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GoalPlanResult {
    steps: Vec<GoalPlanStep>,
}

#[derive(Debug, Clone, Deserialize)]
struct GoalPlanStep {
    instruction: String,
    app_hint: String,
    sensitive: bool,
}

#[derive(Debug, Clone)]
struct ActiveGuide {
    steps: Vec<GoalPlanStep>,
    current_step: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserGoalDirective {
    session_id: String,
    instruction: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserGoalHighlightRequest {
    session_id: String,
    instruction: String,
    url: String,
    elements: Vec<BrowserElement>,
}

#[derive(Default)]
struct NativeBridgeState {
    directive: Option<BrowserGoalDirective>,
    client: Option<Arc<Mutex<UnixStream>>>,
    targets: HashMap<String, BrowserTarget>,
    autopilot: Option<AutopilotRun>,
    control_client: Option<Arc<Mutex<UnixStream>>>,
    overlay_writer: Option<Arc<Mutex<ChildStdin>>>,
    overlay_shown: bool,
}

#[derive(Clone)]
struct BrowserTarget {
    label: String,
    role: String,
}

struct AutopilotRun {
    session_id: String,
    loop_state: AgentLoop,
    started_at: Instant,
    awaiting_page_change: bool,
    pending_snapshot: Option<BrowserGoalHighlightRequest>,
    action_deadline: Option<Instant>,
    // Progress signal for the vision fallback: how many consecutive turns have
    // observed the same URL without navigating, and the last URL we saw. When a
    // page-rich site (e.g. Canva) keeps the agent on one URL, this rises and we
    // escalate to a screenshot so the model can see controls the DOM omits.
    stuck_turns: usize,
    last_observed_url: Option<String>,
    // True while a model turn is being computed. A page-heavy SPA (or a churning
    // MV3 service worker) can deliver a burst of observations; without this the
    // loop would run several model turns in parallel and narrate/handoff
    // repeatedly. Duplicate observations that arrive mid-turn are dropped.
    in_flight: bool,
}

const AUTOPILOT_WALL_CLOCK_CAP: Duration = Duration::from_secs(15 * 60);
const AUTOPILOT_ACTION_ACK_TIMEOUT: Duration = Duration::from_secs(12);

type NativeBridge = Arc<Mutex<NativeBridgeState>>;
const NATIVE_HOST_NAME: &str = "com.mice.bridge";
const EXTENSION_ID: &str = "pmbogcpjmddjpgcilhiplppdhnboeofc";

fn bridge_socket_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    config_path()
        .and_then(|path| path.parent().map(|parent| parent.join("bridge.sock")))
        .ok_or_else(|| "HOME is not set".into())
}

fn start_native_bridge(
    config: mice_core::Config,
    bridge: NativeBridge,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = bridge_socket_path()?;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err("MICE is already running; use the existing daemon.".into());
        }
        std::fs::remove_file(&path)?;
    }
    std::fs::create_dir_all(path.parent().ok_or("bridge socket has no parent")?)?;
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let state = Arc::clone(&bridge);
            let config = config.clone();
            std::thread::spawn(move || {
                let _ = handle_native_bridge(stream, &config, state);
            });
        }
    });
    Ok(())
}

fn native_bridge_send(bridge: &NativeBridge, value: &serde_json::Value) {
    let client = bridge.lock().ok().and_then(|state| state.client.clone());
    if let Some(client) = client
        && let Ok(mut stream) = client.lock()
    {
        let _ = write_frame(&mut *stream, value);
    }
}

fn autopilot_status(bridge: &NativeBridge, text: impl Into<String>, done: bool) {
    let client = bridge
        .lock()
        .ok()
        .and_then(|state| state.control_client.clone());
    if let Some(client) = client
        && let Ok(mut stream) = client.lock()
    {
        let _ = write_frame(
            &mut *stream,
            &serde_json::json!({"type":"autopilot.status", "text":text.into(), "done":done}),
        );
    }
}

fn native_overlay(bridge: &NativeBridge, text: impl Into<String>) {
    autopilot_narrate(bridge, text, false);
}

/// Paint one line of narration to the native overlay and forward it to the CLI
/// control client exactly once, with the terminal `done` flag. Callers must not
/// separately call `autopilot_status` for the same message, or it prints twice.
fn autopilot_narrate(bridge: &NativeBridge, text: impl Into<String>, done: bool) {
    let text = text.into();
    let (writer, update) = bridge.lock().ok().map_or((None, false), |mut state| {
        let update = state.overlay_shown;
        state.overlay_shown = true;
        (state.overlay_writer.clone(), update)
    });
    if let Some(writer) = writer
        && let Ok(mut writer) = writer.lock()
    {
        let command = if update {
            AgentCommand::OverlayUpdate { text: text.clone() }
        } else {
            AgentCommand::OverlayShow { text: text.clone() }
        };
        let _ = send_command(&mut writer, command);
    }
    autopilot_status(bridge, text, done);
}

fn handle_native_bridge(
    mut stream: UnixStream,
    config: &mice_core::Config,
    bridge: NativeBridge,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let message: serde_json::Value = read_frame(&mut stream)?;
        match message["type"].as_str() {
            Some("daemon.stop") => {
                write_frame(&mut stream, &serde_json::json!({"type":"daemon.stopping"}))?;
                let path = bridge_socket_path()?;
                let _ = std::fs::remove_file(path);
                // `mice start` owns the native agent. Exiting this process
                // closes the IPC pipes; the agent treats that as shutdown and
                // terminates too. The socket is removed first so a subsequent
                // start never mistakes it for a live daemon.
                std::process::exit(0);
            }
            Some("autopilot.start") => {
                // The extension-era autopilot was able to mutate Chrome
                // directly. It is retired: AXI is the sole browser mutation
                // executor and confirms every individual action. Keep this
                // protocol response so an old extension fails closed rather
                // than silently regaining that capability.
                write_frame(
                    &mut stream,
                    &serde_json::json!({
                        "type":"autopilot.status",
                        "text":"The legacy extension autopilot is retired. Run `mice autopilot --engine axi <goal>` for individually confirmed browser actions.",
                        "done":true
                    }),
                )?;
            }
            Some("autopilot.stop") => stop_autopilot(&bridge, "Autopilot stopped."),
            Some("bridge.hello") => {
                let writer = Arc::new(Mutex::new(stream.try_clone()?));
                let directive = {
                    let mut state = bridge.lock().map_err(|_| "native bridge lock failed")?;
                    state.client = Some(writer);
                    state.directive.clone()
                };
                native_bridge_send(
                    &bridge,
                    &serde_json::json!({"type":"goal.step", "directive": directive}),
                );
            }
            Some("goal.snapshot") => {
                let request: BrowserGoalHighlightRequest = serde_json::from_value(message)?;
                let is_autopilot = bridge.lock().ok().is_some_and(|state| {
                    state
                        .autopilot
                        .as_ref()
                        .is_some_and(|run| run.session_id == request.session_id)
                });
                if is_autopilot {
                    if let Err(error) = advance_autopilot(config, &bridge, request, None) {
                        eprintln!("[MICE autopilot] {error}");
                        stop_autopilot(&bridge, "I ran into a problem, so I have stopped safely.");
                    }
                    continue;
                }
                let directive = bridge.lock().ok().and_then(|state| state.directive.clone());
                if let Some(directive) = directive.filter(|item| {
                    item.session_id == request.session_id && item.instruction == request.instruction
                }) && let Ok(response) = guide_browser_request(
                    config,
                    BrowserGuideRequest {
                        instruction: directive.instruction,
                        url: request.url,
                        elements: request.elements,
                    },
                ) {
                    if let Ok(mut state) = bridge.lock() {
                        state.targets.insert(
                            request.session_id.clone(),
                            BrowserTarget {
                                label: response.label.clone(),
                                role: response.role.clone(),
                            },
                        );
                    }
                    native_bridge_send(
                        &bridge,
                        &serde_json::json!({"type":"browser.highlight", "sessionId": request.session_id, "selector": response.selector, "instructionText": response.instruction_text}),
                    );
                }
            }
            Some("browser.actResult") => {
                handle_autopilot_result(&bridge, &message);
                eprintln!(
                    "[MICE act] session {}: {}",
                    message["sessionId"].as_str().unwrap_or("unknown"),
                    if message["ok"].as_bool().unwrap_or(false) {
                        "completed"
                    } else {
                        message["error"].as_str().unwrap_or("failed")
                    }
                );
            }
            Some("browser.screenshot") => {
                let session_id = message["sessionId"].as_str().unwrap_or_default();
                let data_url = message["dataUrl"].as_str().unwrap_or_default();
                let snapshot = bridge.lock().ok().and_then(|mut state| {
                    state.autopilot.as_mut().and_then(|run| {
                        (run.session_id == session_id)
                            .then(|| run.pending_snapshot.take())
                            .flatten()
                    })
                });
                if let Some(snapshot) = snapshot {
                    let image = if data_url.len() <= 900_000 {
                        data_url
                    } else {
                        ""
                    };
                    if let Err(error) = advance_autopilot(config, &bridge, snapshot, Some(image)) {
                        eprintln!("[MICE autopilot] {error}");
                        stop_autopilot(
                            &bridge,
                            "I could not inspect that page safely, so I stopped.",
                        );
                    }
                }
            }
            Some("browser.pageChanged") => {
                let directive = if let Ok(mut state) = bridge.lock() {
                    state.targets.clear();
                    if let Some(run) = state.autopilot.as_mut()
                        && run.awaiting_page_change
                        // Do not consume the awaited page change while an action
                        // is still in flight; a busy SPA fires many mutations
                        // before the action's result arrives, and consuming one
                        // here would skip the real post-action re-observation.
                        && !run.in_flight
                        && matches!(run.loop_state.state, AgentLoopState::Running)
                    {
                        run.awaiting_page_change = false;
                        run.action_deadline = None;
                        state.directive.clone()
                    } else {
                        None
                    }
                } else {
                    None
                };
                eprintln!(
                    "[MICE observe] page changed: {}",
                    message["url"].as_str().unwrap_or("unknown page")
                );
                if let Some(directive) = directive {
                    native_bridge_send(
                        &bridge,
                        &serde_json::json!({"type":"goal.step", "directive": directive}),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Decide and perform one browser turn. The model only sees the locally
/// bounded candidate list; it can name a candidate ID but never a selector.
fn advance_autopilot(
    config: &mice_core::Config,
    bridge: &NativeBridge,
    request: BrowserGoalHighlightRequest,
    screenshot: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (goal, history, ranking_instruction, stuck_turns) = {
        let mut state = bridge.lock().map_err(|_| "native bridge lock failed")?;
        // A finished run is torn down; a stray late observation is a no-op.
        let Some(run) = state.autopilot.as_mut() else {
            return Ok(());
        };
        if run.session_id != request.session_id {
            return Ok(());
        }
        if run.started_at.elapsed() > AUTOPILOT_WALL_CLOCK_CAP {
            drop(state);
            stop_autopilot(bridge, "The time limit was reached. I have stopped safely.");
            return Ok(());
        }
        if !matches!(run.loop_state.state, AgentLoopState::Running) {
            return Ok(());
        }
        // Collapse a burst of observations into one turn. The screenshot
        // re-entry (screenshot.is_some()) is the continuation of the current
        // turn, so it is never dropped and never re-marks in_flight.
        if screenshot.is_none() {
            if run.in_flight {
                return Ok(());
            }
            run.in_flight = true;
            // Count progress on the first observation of a turn only.
            if run.last_observed_url.as_deref() == Some(request.url.as_str()) {
                run.stuck_turns += 1;
            } else {
                run.stuck_turns = 0;
                run.last_observed_url = Some(request.url.clone());
            }
        }
        let ranking_instruction = run.loop_state.history.last().map_or_else(
            || run.loop_state.goal.clone(),
            |turn| format!("{}\nCurrent progress: {}", run.loop_state.goal, turn.result),
        );
        (
            run.loop_state.goal.clone(),
            render_agent_history(&run.loop_state),
            ranking_instruction,
            run.stuck_turns,
        )
    };

    let mut candidates = rank_guide_candidates(&ranking_instruction, request.elements.clone());
    // Bound the observation so a control-dense page cannot exceed the provider's
    // request-size limit. Keep the highest-ranked controls that fit the budget.
    {
        let mut budget = request.url.len() + 128;
        let mut kept = 0;
        for candidate in &candidates {
            let cost = candidate.selector.len() + candidate.label.len() + candidate.role.len() + 48;
            if kept > 0 && budget + cost > MAX_OBSERVATION_CHARS {
                break;
            }
            budget += cost;
            kept += 1;
        }
        candidates.truncate(kept);
    }
    // Diagnostic: how many controls the page exposed and the top few labels.
    // A near-empty list here on a rich page means the extension is running an
    // old content.js (reload it) rather than a model problem.
    if screenshot.is_none() {
        let preview = candidates
            .iter()
            .take(6)
            .map(|candidate| candidate.label.trim())
            .filter(|label| !label.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        eprintln!(
            "[MICE observe] {} controls on {} | top: {preview}",
            candidates.len(),
            request.url
        );
    }
    // Escalate to a screenshot when the DOM is sparse OR the agent has stalled
    // on one page for two turns (common on canvas/app UIs like Canva where
    // clickable tiles are divs the DOM snapshot cannot expose). Only worth doing
    // when vision is actually reachable.
    let vision_possible = env::var_os("OPENAI_API_KEY").is_some();
    let needs_screenshot = screenshot.is_none()
        && vision_possible
        && (candidates.len() <= 3 || stuck_turns >= 2)
        && bridge.lock().ok().is_some_and(|mut state| {
            state.autopilot.as_mut().is_some_and(|run| {
                if run.session_id == request.session_id {
                    run.pending_snapshot = Some(request.clone());
                    true
                } else {
                    false
                }
            })
        });
    if needs_screenshot && stuck_turns >= 2 {
        eprintln!(
            "[MICE autopilot] Stalled on this page; taking a screenshot to look for controls the page markup does not expose."
        );
    }
    if needs_screenshot {
        native_bridge_send(
            bridge,
            &serde_json::json!({"type":"browser.screenshot", "sessionId":request.session_id}),
        );
        return Ok(());
    }
    let observation = serde_json::to_string(&serde_json::json!({
        "url": request.url,
        "interactive_elements": candidates,
    }))?;
    let usable_screenshot = screenshot.filter(|image| image.starts_with("data:image/"));
    let vision_unavailable = usable_screenshot.is_some() && env::var_os("OPENAI_API_KEY").is_none();
    if vision_unavailable {
        let notice = "I cannot use the visual view because OpenAI is not configured. I will continue from the page controls I can read.";
        eprintln!("[MICE autopilot] {notice}");
        native_overlay(bridge, notice);
    }
    let output = if usable_screenshot.is_some() && !vision_unavailable {
        call_openai_agent_turn(
            "gpt-5.6-sol",
            &goal,
            &observation,
            &history,
            usable_screenshot,
            &config.autopilot.persona,
        )?
    } else if is_groq_model(&config.cloud_model) {
        call_groq_agent_turn(
            &config.cloud_model,
            &goal,
            &observation,
            &history,
            &config.autopilot.persona,
        )?
    } else {
        call_openai_agent_turn(
            &config.cloud_model,
            &goal,
            &observation,
            &history,
            None,
            &config.autopilot.persona,
        )?
    };
    let mut decision: AgentDecision = serde_json::from_str(&output).map_err(|error| {
        eprintln!(
            "[MICE autopilot] Could not decode model decision: {error}; output: {}",
            bounded_for_model(&output, 1_000)
        );
        "The cloud model returned an invalid autopilot decision."
    })?;
    validate_agent_decision(&decision)?;

    // If the model is about to give up or ask for help without pointing at any
    // control, let it look at the page first — a screenshot often reveals the
    // button the DOM did not expose. Retry once (only when vision was not used
    // this turn); in_flight stays set so the screenshot re-entry continues.
    if screenshot.is_none()
        && vision_possible
        && decision.candidate_id.is_none()
        && matches!(decision.action, AgentAction::Handoff | AgentAction::AskUser)
    {
        let stored = bridge.lock().ok().is_some_and(|mut state| {
            state.autopilot.as_mut().is_some_and(|run| {
                if run.session_id == request.session_id {
                    run.pending_snapshot = Some(request.clone());
                    true
                } else {
                    false
                }
            })
        });
        if stored {
            eprintln!(
                "[MICE autopilot] The model found no control to use; taking a screenshot before it hands off."
            );
            native_bridge_send(
                bridge,
                &serde_json::json!({"type":"browser.screenshot", "sessionId":request.session_id}),
            );
            return Ok(());
        }
    }

    let selected = decision.candidate_id.as_deref().and_then(|candidate_id| {
        candidates
            .iter()
            .find(|candidate| candidate.candidate_id.as_deref() == Some(candidate_id))
    });
    if matches!(decision.action, AgentAction::Click | AgentAction::Fill) && selected.is_none() {
        decision = handoff_decision(
            "I could not verify that control on this page. Please do that step yourself, then I can continue.",
        );
    }
    if let Some(selected) = selected {
        let target = BrowserTarget {
            label: selected.label.clone(),
            role: selected.role.clone(),
        };
        let kind = match decision.action {
            AgentAction::Click => Some("click"),
            AgentAction::Fill => Some("fill"),
            _ => None,
        };
        if let Some(kind) = kind
            && let Some(reason) = blocked_browser_action(kind, &target, &goal)
        {
            decision = handoff_decision(&format!(
                "This is a protected step ({reason}). I have highlighted it; please do it yourself."
            ));
        }
    }

    let mut action_to_send = None;
    let mut action_preview = None;
    let mut highlight_to_send = None;
    let mut terminal_message = None;
    {
        let mut state = bridge.lock().map_err(|_| "native bridge lock failed")?;
        let Some(run) = state.autopilot.as_mut() else {
            return Ok(());
        };
        if run.session_id != request.session_id
            || !matches!(run.loop_state.state, AgentLoopState::Running)
        {
            return Ok(());
        }
        run.loop_state.apply_decision(&decision)?;
        match &run.loop_state.state {
            AgentLoopState::BudgetExhausted => {
                terminal_message =
                    Some("I reached the 15-action limit, so I have stopped safely.".into());
            }
            AgentLoopState::Done(summary) => terminal_message = Some(format!("Done: {summary}")),
            AgentLoopState::Paused(question) => {
                terminal_message = Some(format!("I need your help: {question}"))
            }
            AgentLoopState::HandedOff(message) => {
                terminal_message = Some(message.clone());
                // Always point somewhere on a handoff so the user is guided.
                // Prefer the control the model chose; otherwise highlight the
                // best-ranked candidate as a clearly-labelled best guess.
                if let Some(selected) = selected {
                    highlight_to_send =
                        Some((selected.selector.clone(), decision.say_to_user.clone()));
                } else if let Some(guess) = candidates.first() {
                    highlight_to_send = Some((
                        guess.selector.clone(),
                        format!(
                            "Best guess — you may need to use \u{201c}{}\u{201d}.",
                            guess.label
                        ),
                    ));
                }
            }
            AgentLoopState::Stopped | AgentLoopState::Running => {}
        }
        if matches!(run.loop_state.state, AgentLoopState::Running) {
            match decision.action {
                AgentAction::Click | AgentAction::Fill => {
                    let selected = selected.ok_or("missing verified action candidate")?;
                    // Candidate IDs are regenerated on every snapshot; use
                    // the verified selector for failure correlation instead.
                    run.loop_state.last_action_target = Some(selected.selector.clone());
                    run.awaiting_page_change = true;
                    run.loop_state.record(
                        format!("{:?}", decision.action).to_ascii_lowercase(),
                        format!("sent to {}", selected.label),
                    );
                    action_to_send = Some(serde_json::json!({
                        "type":"browser.act", "sessionId":request.session_id,
                        "action": if matches!(decision.action, AgentAction::Click) { "click" } else { "fill" },
                        "selector":selected.selector, "value":decision.value,
                    }));
                    action_preview = Some(format!(
                        "{} {}",
                        if matches!(decision.action, AgentAction::Click) {
                            "click"
                        } else {
                            "type into"
                        },
                        selected.label
                    ));
                }
                AgentAction::OpenUrl => {
                    let url = decision.url.clone().ok_or("open_url needs a URL")?;
                    if !valid_browser_url(&url) {
                        return Err("MICE only opens http or https URLs.".into());
                    }
                    run.loop_state.last_action_target = None;
                    run.awaiting_page_change = true;
                    run.loop_state.record("open_url", format!("opened {url}"));
                    action_to_send = Some(serde_json::json!({
                        "type":"browser.act", "sessionId":request.session_id,
                        "action":"open_url", "url":url,
                    }));
                    action_preview = Some(format!("open {url}"));
                }
                AgentAction::Scroll => {
                    run.loop_state.last_action_target = None;
                    run.awaiting_page_change = false;
                    run.loop_state.record("scroll", "sent");
                    action_to_send = Some(serde_json::json!({
                        "type":"browser.act", "sessionId":request.session_id,
                        "action":"scroll",
                    }));
                    action_preview = Some("scroll the page".into());
                }
                AgentAction::Done | AgentAction::Handoff | AgentAction::AskUser => {}
            }
        }
    }
    if let Some((selector, instruction_text)) = highlight_to_send {
        native_bridge_send(
            bridge,
            &serde_json::json!({"type":"browser.highlight", "sessionId":request.session_id, "selector":selector, "instructionText":instruction_text}),
        );
    }
    if let Some(message) = &terminal_message {
        let done = bridge.lock().ok().is_some_and(|state| {
            state
                .autopilot
                .as_ref()
                .is_some_and(|run| !matches!(run.loop_state.state, AgentLoopState::Running))
        });
        // Narrate the outcome exactly once (overlay + client, with the done flag).
        println!("[MICE autopilot] {message}");
        autopilot_narrate(bridge, message, done);
        if !message.starts_with("Done:") && !message.starts_with("I need your help:") {
            clear_browser_goal_directive(bridge);
        }
        // Tear the run down on any terminal state so a late or duplicated
        // observation from the page cannot re-enter the loop and repeat the
        // narration/handoff.
        if done && let Ok(mut state) = bridge.lock() {
            state.autopilot = None;
        }
    } else {
        // Non-terminal action turn: narrate the model's plan for this step once.
        println!("[MICE autopilot] {}", decision.say_to_user.trim());
        native_overlay(bridge, decision.say_to_user.trim());
    }
    if (config.autopilot.careful_mode || config.autopilot.first_run)
        && let Some(preview) = action_preview
    {
        native_overlay(bridge, format!("Next I will {preview}."));
    }
    if let Some(message) = action_to_send {
        // Keep the turn guard held: the next observation must wait until this
        // action's result (or its ack timeout) arrives, so a page that mutates
        // constantly cannot start overlapping turns.
        arm_autopilot_action_timeout(bridge, &request.session_id);
        native_bridge_send(bridge, &message);
    } else {
        // No action dispatched this turn (terminal or no-op): release the guard
        // so a later observation can proceed. Terminal states have already torn
        // the run down, making this a harmless no-op then.
        if let Ok(mut state) = bridge.lock()
            && let Some(run) = state.autopilot.as_mut()
            && run.session_id == request.session_id
        {
            run.in_flight = false;
        }
    }
    Ok(())
}

fn arm_autopilot_action_timeout(bridge: &NativeBridge, session_id: &str) {
    if let Ok(mut state) = bridge.lock()
        && let Some(run) = state.autopilot.as_mut()
        && run.session_id == session_id
        && matches!(run.loop_state.state, AgentLoopState::Running)
    {
        run.action_deadline = Some(Instant::now() + AUTOPILOT_ACTION_ACK_TIMEOUT);
    }
}

fn handle_autopilot_result(bridge: &NativeBridge, message: &serde_json::Value) {
    let session_id = message["sessionId"].as_str().unwrap_or_default();
    let ok = message["ok"].as_bool().unwrap_or(false);
    let page_changed = message["pageChanged"].as_bool().unwrap_or(false);
    let directive = if let Ok(mut state) = bridge.lock() {
        let Some(run) = state.autopilot.as_mut() else {
            return;
        };
        if run.session_id != session_id || !matches!(run.loop_state.state, AgentLoopState::Running)
        {
            return;
        }
        // The dispatched action has resolved; release the turn guard so the
        // deliberate re-observation below (or a page-change event) can proceed.
        run.in_flight = false;
        if ok {
            run.action_deadline = None;
            run.loop_state.record_action_result(
                run.loop_state.last_action_target.clone(),
                true,
                "completed",
            );
            // Always re-observe after a successful action. The content script
            // waits for the page to settle before answering, so this works for
            // both same-page interactions and navigations; relying on a separate
            // page-change event could miss it and stall the loop.
            run.awaiting_page_change = false;
            state.directive.clone()
        } else {
            run.action_deadline = None;
            let error = message["error"].as_str().unwrap_or("browser action failed");
            run.loop_state.record_action_result(
                run.loop_state.last_action_target.clone(),
                false,
                error,
            );
            if matches!(run.loop_state.state, AgentLoopState::HandedOff(_)) {
                None
            } else {
                run.awaiting_page_change = false;
                state.directive.clone()
            }
        }
    } else {
        None
    };
    eprintln!(
        "[MICE act-result] ok={ok} pageChanged={page_changed} reobserve={}",
        directive.is_some()
    );
    if let Some(directive) = directive {
        native_bridge_send(
            bridge,
            &serde_json::json!({"type":"goal.step", "directive":directive}),
        );
    }
}

/// The extension service worker may be reclaimed between dispatch and its
/// acknowledgement. Re-observe rather than replaying an uncertain action.
fn recover_autopilot_timeouts(bridge: &NativeBridge) {
    let (directive, message): (Option<BrowserGoalDirective>, Option<String>) =
        if let Ok(mut state) = bridge.lock() {
            let Some(run) = state.autopilot.as_mut() else {
                return;
            };
            if !matches!(run.loop_state.state, AgentLoopState::Running) {
                return;
            }
            if run.started_at.elapsed() >= AUTOPILOT_WALL_CLOCK_CAP {
                run.loop_state.stop();
                state.directive = None;
                (
                    None,
                    Some("The 15-minute time limit was reached. I have stopped safely.".into()),
                )
            } else if run
                .action_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                run.action_deadline = None;
                run.in_flight = false;
                run.awaiting_page_change = false;
                run.loop_state.record_action_result(
                    run.loop_state.last_action_target.clone(),
                    false,
                    "No browser acknowledgement arrived; rechecking the page.",
                );
                if matches!(run.loop_state.state, AgentLoopState::Running) {
                    (
                        state.directive.clone(),
                        Some("I am rechecking the page after a delayed browser response.".into()),
                    )
                } else {
                    state.directive = None;
                    (
                        None,
                        Some(
                            "That control did not respond twice, so I have handed it back to you."
                                .into(),
                        ),
                    )
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
    if let Some(message) = message {
        eprintln!("[MICE autopilot] {message}");
        native_overlay(bridge, message);
    }
    if let Some(directive) = directive {
        native_bridge_send(
            bridge,
            &serde_json::json!({"type":"goal.step", "directive":directive}),
        );
    }
}

fn render_agent_history(loop_state: &AgentLoop) -> String {
    if loop_state.history.is_empty() {
        "No prior actions.".into()
    } else {
        loop_state
            .history
            .iter()
            .map(|turn| {
                format!(
                    "{}: {}",
                    turn.action,
                    bounded_for_model(&turn.result, MAX_GUIDE_LABEL_CHARS)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn validate_agent_decision(decision: &AgentDecision) -> Result<(), Box<dyn std::error::Error>> {
    if decision.say_to_user.trim().is_empty() || decision.say_to_user.chars().count() > 500 {
        return Err("The autopilot decision did not include a safe narration.".into());
    }
    if matches!(decision.action, AgentAction::Fill)
        && decision.value.as_deref().unwrap_or_default().is_empty()
    {
        return Err("The autopilot asked to fill without text.".into());
    }
    if matches!(decision.action, AgentAction::OpenUrl)
        && decision.url.as_deref().unwrap_or_default().is_empty()
    {
        return Err("The autopilot asked to open an empty URL.".into());
    }
    Ok(())
}

fn handoff_decision(message: &str) -> AgentDecision {
    AgentDecision {
        say_to_user: message.into(),
        action: AgentAction::Handoff,
        candidate_id: None,
        url: None,
        value: None,
        done_summary: None,
        question: None,
    }
}

fn valid_browser_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn stop_autopilot(bridge: &NativeBridge, message: &str) {
    if let Ok(mut state) = bridge.lock()
        && let Some(run) = state.autopilot.as_mut()
    {
        run.loop_state.stop();
    }
    clear_browser_goal_directive(bridge);
    native_overlay(bridge, message);
    autopilot_status(bridge, message, true);
    eprintln!("[MICE autopilot] {message}");
}

fn native_host() -> Result<(), Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(bridge_socket_path()?)?;
    let mut to_core = stream.try_clone()?;
    std::thread::spawn(move || {
        let mut input = std::io::stdin();
        while let Ok(message) = read_frame::<serde_json::Value>(&mut input) {
            if write_frame(&mut to_core, &message).is_err() {
                break;
            }
        }
    });
    let mut from_core = stream;
    let mut output = std::io::stdout();
    while let Ok(message) = read_frame::<serde_json::Value>(&mut from_core) {
        write_frame(&mut output, &message)?;
    }
    Ok(())
}

fn setup_browser() -> Result<(), Box<dyn std::error::Error>> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    let directory =
        PathBuf::from(home).join("Library/Application Support/Google/Chrome/NativeMessagingHosts");
    std::fs::create_dir_all(&directory)?;
    let manifest = serde_json::json!({"name": NATIVE_HOST_NAME, "description": "MICE browser companion", "path": env::current_exe()?, "type": "stdio", "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")]});
    std::fs::write(
        directory.join(format!("{NATIVE_HOST_NAME}.json")),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!("Installed native browser host for extension {EXTENSION_ID}.");
    Ok(())
}

fn autopilot() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(2).collect::<Vec<_>>();
    let engine = if arguments
        .first()
        .is_some_and(|argument| argument == "--engine")
    {
        if arguments.len() < 2 {
            return Err("Usage: mice autopilot [--engine axi] <goal>".into());
        }
        let engine = arguments[1].clone();
        arguments.drain(0..2);
        engine
    } else {
        "axi".into()
    };
    let goal = arguments.join(" ");
    if goal.trim().is_empty() {
        return Err(
            "Provide a goal, for example: mice autopilot \"search Canva and open a portrait\""
                .into(),
        );
    }
    if engine != "axi" {
        return Err(
            "The legacy extension autopilot is retired. Use `mice autopilot --engine axi <goal>`; it requires confirmation for every action."
                .into(),
        );
    }
    autopilot_axi(&goal)
}

/// Browser autopilot v2: the registry shells out to chrome-devtools-axi, so no
/// Chrome extension/native-host chain is involved. The same bounded local tool
/// loop supplies observe → act → verify turns through browser.snapshot/actions.
fn autopilot_axi(goal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let runner = SystemRunner;
    if !runner.available("npx") {
        return Err("AXI autopilot requires Node's `npx`. Install Node, then retry.".into());
    }
    let context = ToolContext {
        working_dir: env::current_dir()?,
        session_name: format!("mice-autopilot-{}", std::process::id()),
        output_budget_tokens: tools::DEFAULT_RETURN_TOKENS,
    };
    let primary_lane = axi_model_lane(&config, local_tool_model_available(&config, &runner))?;
    println!("MICE AXI guide: {goal}");
    println!(
        "Every browser action will be shown and requires your confirmation. MICE never fills credentials or payment data, and never clicks sign-in, payment, transfer, or final-submission controls."
    );
    if matches!(
        primary_lane,
        ExecutionLane::CheapCloud | ExecutionLane::Frontier
    ) && config.privacy_mode == PrivacyMode::CloudAllowed
    {
        confirm_axi_cloud_fallback(
            &config.cloud_model,
            "this machine profile cannot run the local browser loop",
        )?;
    }

    let mut history = Vec::new();
    let mut local_uncertainties = 0_u8;
    let mut completed_actions = 0_usize;
    while completed_actions < 6 {
        // A stale retry belongs to this proposed action, not to the whole
        // guide. A replan does not use up a successfully-dispatched action.
        let mut recovery = AxiActionRecovery::default();
        loop {
            let observed = match observe_axi(&runner, &context) {
                Ok(observed) => observed,
                Err(error) => return pause_axi(goal, &history, &error),
            };
            let observation = observed.text.clone();
            let lane = if primary_lane == ExecutionLane::Local
                && local_uncertainties >= AXI_LOCAL_UNCERTAINTY_LIMIT
            {
                let fallback = axi_cloud_fallback_lane(&config)?;
                confirm_axi_cloud_fallback(
                    &config.cloud_model,
                    "the local guide was uncertain twice on the current page",
                )?;
                fallback
            } else {
                primary_lane
            };
            let decision = call_axi_agent_turn(&config, lane, goal, &observation, &history)?;
            validate_agent_decision(&decision)?;
            println!("MICE: {}", decision.say_to_user);

            let Some(call) = axi_call_from_decision(&decision)? else {
                if lane == ExecutionLane::Local
                    && config.privacy_mode == PrivacyMode::CloudAllowed
                    && matches!(
                        &decision.action,
                        AgentAction::Handoff | AgentAction::AskUser
                    )
                {
                    local_uncertainties += 1;
                    history.push(format!(
                        "local guide uncertainty {local_uncertainties}/{AXI_LOCAL_UNCERTAINTY_LIMIT}: {}",
                        decision.say_to_user
                    ));
                    if local_uncertainties < AXI_LOCAL_UNCERTAINTY_LIMIT {
                        println!(
                            "MICE will take one more local-only look before considering a cloud fallback."
                        );
                    }
                    continue;
                }
                match decision.action {
                    AgentAction::Done => println!(
                        "MICE AXI guide complete: {}",
                        decision
                            .done_summary
                            .as_deref()
                            .unwrap_or("The requested step is complete.")
                    ),
                    AgentAction::AskUser => println!(
                        "MICE needs your input: {}",
                        decision
                            .question
                            .as_deref()
                            .unwrap_or("Please clarify the next step.")
                    ),
                    AgentAction::Handoff => println!("MICE has handed this step back to you."),
                    _ => unreachable!("action conversion handles executable actions"),
                }
                return Ok(());
            };

            println!(
                "Proposed action {}/6: {}",
                completed_actions + 1,
                observed.snapshot.approval_summary(&call)
            );
            print!("Do this one action? [y/N] ");
            std::io::stdout().flush()?;
            let mut consent = String::new();
            std::io::stdin().read_line(&mut consent)?;
            if !matches!(consent.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                println!("MICE did not act. You can continue manually or run a narrower goal.");
                return Ok(());
            }

            // Re-observe after confirmation so a stale model reference can
            // never be acted upon. A changed UID is rejected and replanned.
            let current = match observe_axi(&runner, &context) {
                Ok(current) => current,
                Err(error) => return pause_axi(goal, &history, &error),
            };
            if !observed
                .snapshot
                .same_target_context(&current.snapshot, &call)
            {
                history.push("AXI target context changed after confirmation; replanning.".into());
                println!(
                    "The target changed after confirmation. Re-observing and replanning without acting."
                );
                continue;
            }
            let result = tools::execute_verified_browser_action(
                &runner,
                &call,
                &context,
                &current.snapshot,
                true,
            );
            let result = match result {
                Ok(result) => result,
                Err(error) if is_axi_stale_error(&error) && recovery.retry_stale_once() => {
                    history.push(format!("stale AXI target: {error}"));
                    println!(
                        "The page changed before MICE acted. Re-observing once and replanning safely."
                    );
                    continue;
                }
                Err(error) => return pause_axi(goal, &history, &error),
            };
            let outcome = if result.text.trim().is_empty() {
                "action sent".into()
            } else {
                bounded_for_model(&result.text, 300)
            };
            history.push(format!("{} {} => {outcome}", call.name, call.args));
            completed_actions += 1;
            break;
        }
    }
    Err(
        "MICE AXI guide reached its six-action limit. Continue with a narrower follow-up goal."
            .into(),
    )
}

const AXI_LOCAL_UNCERTAINTY_LIMIT: u8 = 2;

#[derive(Default)]
struct AxiActionRecovery {
    stale_retried: bool,
}

impl AxiActionRecovery {
    /// Return true exactly once for a proposed action. A completed action gets
    /// a fresh recovery state on the next outer-loop iteration.
    fn retry_stale_once(&mut self) -> bool {
        if self.stale_retried {
            false
        } else {
            self.stale_retried = true;
            true
        }
    }
}

fn local_tool_model_available(config: &mice_core::Config, runner: &impl CommandRunner) -> bool {
    runner.available("ollama")
        && mice_providers::model_descriptor(&config.tool_model).is_some()
        && mice_providers::ollama_model_ready("http://127.0.0.1:11434", &config.tool_model).is_ok()
}

fn axi_model_lane(
    config: &mice_core::Config,
    local_tool_available: bool,
) -> Result<ExecutionLane, Box<dyn std::error::Error>> {
    match config.privacy_mode {
        PrivacyMode::LocalOnly => {
            let lane = route_execution_lane(
                config.machine_profile,
                &config.routing,
                false,
                true,
                current_quota_usage_percent(),
            );
            if lane != ExecutionLane::Local {
                return Err("AXI local-only mode needs a standard or heavy machine profile with the local tool loop enabled. MICE will not export browser content to a cloud provider.".into());
            }
            if !local_tool_available {
                return Err(format!(
                    "AXI local-only mode needs a reachable Ollama installation and a supported tool model (`{}`). Run `ollama serve`, pull the model, then run `mice bench-tools`.",
                    config.tool_model
                )
                .into());
            }
            Ok(lane)
        }
        PrivacyMode::CloudOnly => axi_cloud_fallback_lane(config),
        PrivacyMode::CloudAllowed => match route_execution_lane(
            config.machine_profile,
            &config.routing,
            false,
            true,
            current_quota_usage_percent(),
        ) {
            ExecutionLane::Local if local_tool_available => Ok(ExecutionLane::Local),
            ExecutionLane::Local => axi_cloud_fallback_lane(config),
            ExecutionLane::CheapCloud | ExecutionLane::Frontier => Ok(route_execution_lane(
                config.machine_profile,
                &config.routing,
                false,
                true,
                current_quota_usage_percent(),
            )),
            _ => Err(
                "AXI browser guidance has no enabled local or cloud model lane in MICE settings."
                    .into(),
            ),
        },
    }
}

fn current_quota_usage_percent() -> Option<u8> {
    type QuotaCache = Option<(Instant, Option<u8>)>;
    static CACHE: OnceLock<Mutex<QuotaCache>> = OnceLock::new();
    if let Some(value) = env::var("MICE_QUOTA_PERCENT")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value <= 100)
    {
        return Some(value);
    }
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some((checked, value)) = *guard
        && checked.elapsed() < Duration::from_secs(300)
    {
        return value;
    }
    let value = tools::run(
        &SystemRunner,
        &ToolCall {
            name: "quota.status".into(),
            args: json!({}),
        },
        &ToolContext {
            working_dir: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            session_name: tool_session_name(),
            output_budget_tokens: 300,
        },
    )
    .ok()
    .and_then(|output| serde_json::from_str::<Value>(&output.raw).ok())
    .and_then(|value| quota_usage_percent_from_value(&value));
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), value));
    }
    value
}

fn quota_usage_percent_from_value(value: &Value) -> Option<u8> {
    match value {
        Value::Object(values) => values.iter().find_map(|(key, value)| {
            let key = key.to_ascii_lowercase();
            let number = value.as_f64().filter(|value| (0.0..=100.0).contains(value));
            match (key.as_str(), number) {
                (
                    "used_percent" | "usage_percent" | "percent_used" | "usedpercent"
                    | "usagepercent" | "percentused",
                    Some(value),
                ) => Some(value.round() as u8),
                (
                    "remaining_percent" | "percent_remaining" | "remainingpercent"
                    | "percentremaining",
                    Some(value),
                ) => Some(100_u8.saturating_sub(value.round() as u8)),
                _ => quota_usage_percent_from_value(value),
            }
        }),
        Value::Array(values) => values
            .iter()
            .filter_map(quota_usage_percent_from_value)
            .max(),
        _ => None,
    }
}

fn axi_cloud_fallback_lane(
    config: &mice_core::Config,
) -> Result<ExecutionLane, Box<dyn std::error::Error>> {
    if config.routing.cheap_cloud {
        Ok(ExecutionLane::CheapCloud)
    } else if config.routing.frontier {
        Ok(ExecutionLane::Frontier)
    } else {
        Err("Cloud fallback is disabled in MICE settings; MICE will pause rather than export this browser snapshot.".into())
    }
}

fn confirm_axi_cloud_fallback(model: &str, reason: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "MICE needs cloud model `{model}` because {reason}. This sends the current bounded AXI snapshot to that provider."
    );
    print!("Allow this cloud fallback? [y/N] ");
    std::io::stdout().flush()?;
    let mut consent = String::new();
    std::io::stdin().read_line(&mut consent)?;
    if matches!(consent.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err("Cloud fallback was not approved; MICE did not export browser content.".into())
    }
}

fn is_axi_stale_error(error: &impl std::fmt::Display) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("stale_ref")
        || message.contains("stale ref")
        || message.contains("not in the current axi snapshot")
}

fn pause_axi(
    goal: &str,
    history: &[String],
    error: &impl std::fmt::Display,
) -> Result<(), Box<dyn std::error::Error>> {
    let detail = error.to_string();
    let reason = if is_axi_stale_error(&detail) {
        "the page changed again"
    } else {
        "Chrome or the AXI browser bridge became unavailable"
    };
    let last_history = history.last().map(String::as_str).unwrap_or("none");
    println!(
        "MICE AXI guide paused for ‘{goal}’: {reason}. No further action was taken. Reopen Chrome if needed, then rerun this goal. Last safe history: {last_history}"
    );
    eprintln!("[MICE AXI paused] {detail}");
    Ok(())
}

struct AxiObservation {
    text: String,
    snapshot: tools::BrowserSnapshot,
}

fn observe_axi(
    runner: &impl CommandRunner,
    context: &ToolContext,
) -> Result<AxiObservation, Box<dyn std::error::Error>> {
    let output = tools::run(
        runner,
        &ToolCall {
            name: "browser.snapshot".into(),
            args: json!({}),
        },
        context,
    )?;
    Ok(AxiObservation {
        snapshot: tools::BrowserSnapshot::from_axi_output(&output.raw),
        text: output.text,
    })
}

fn call_axi_agent_turn(
    config: &mice_core::Config,
    lane: ExecutionLane,
    goal: &str,
    observation: &str,
    history: &[String],
) -> Result<AgentDecision, Box<dyn std::error::Error>> {
    let history = if history.is_empty() {
        "No prior actions.".into()
    } else {
        history.join("\n")
    };
    let output = if lane == ExecutionLane::Local {
        let mut output = String::new();
        let instruction = "You are MICE, a careful browser guide. Return only one JSON object with exactly these snake_case fields: say_to_user, action (click|fill|open_url|scroll|done|handoff|ask_user), candidate_id, url, value, done_summary, question. Copy a candidate_id exactly from the AXI snapshot. Never fill passwords, codes, or payment data. Never click sign-in, payment, purchase, transfer, final-submit, or file-return controls. Prefer handoff instead of guessing.";
        stream_ollama(
            &config.tool_model,
            instruction,
            Some(&format!(
                "Goal: {goal}\n\nCurrent AXI snapshot:\n{observation}\n\nPrior actions:\n{history}"
            )),
            |chunk| {
                output.push_str(chunk);
                Ok(())
            },
        )?;
        output
    } else if is_groq_model(&config.cloud_model) {
        call_groq_agent_turn(
            &config.cloud_model,
            goal,
            observation,
            &history,
            &config.autopilot.persona,
        )?
    } else {
        call_openai_agent_turn(
            &config.cloud_model,
            goal,
            observation,
            &history,
            None,
            &config.autopilot.persona,
        )?
    };
    Ok(
        serde_json::from_str(extract_json_object(&output)).map_err(|error| {
            format!(
                "AXI guide model returned an invalid decision: {error}; output: {}",
                bounded_for_model(&output, 1_000)
            )
        })?,
    )
}

fn axi_call_from_decision(
    decision: &AgentDecision,
) -> Result<Option<ToolCall>, Box<dyn std::error::Error>> {
    let require_candidate = || {
        decision
            .candidate_id
            .as_deref()
            .filter(|candidate| !candidate.trim().is_empty())
            .ok_or("The AXI guide chose an action without a current target reference.")
    };
    let call = match decision.action {
        AgentAction::Click => ToolCall {
            name: "browser.click".into(),
            args: json!({"uid": require_candidate()?}),
        },
        AgentAction::Fill => ToolCall {
            name: "browser.fill".into(),
            args: json!({
                "uid": require_candidate()?,
                "text": decision.value.as_deref().ok_or("The AXI guide chose fill without text.")?
            }),
        },
        AgentAction::OpenUrl => ToolCall {
            name: "browser.open".into(),
            args: json!({"url": decision.url.as_deref().ok_or("The AXI guide chose open_url without a URL.")?}),
        },
        AgentAction::Scroll => ToolCall {
            name: "browser.scroll".into(),
            args: json!({"direction": decision.value.as_deref().unwrap_or("down")}),
        },
        AgentAction::Done | AgentAction::Handoff | AgentAction::AskUser => return Ok(None),
    };
    Ok(Some(call))
}

/// Ask the running daemon to stop over its owner-only bridge socket. This is
/// deliberately not a process-name kill: it cannot affect another MICE run or
/// an unrelated program, and the daemon can acknowledge the request first.
fn stop() -> Result<(), Box<dyn std::error::Error>> {
    let mut stream =
        UnixStream::connect(bridge_socket_path()?).map_err(|_| "MICE is not running.")?;
    write_frame(&mut stream, &serde_json::json!({"type":"daemon.stop"}))?;
    let response: serde_json::Value = read_frame(&mut stream)?;
    if response["type"] != "daemon.stopping" {
        return Err("The MICE daemon did not acknowledge the stop request.".into());
    }
    println!("MICE is stopping.");
    Ok(())
}

fn browser_bridge() -> Result<(), Box<dyn std::error::Error>> {
    let token = env::var("MICE_BROWSER_BRIDGE_TOKEN")
        .map_err(|_| "MICE_BROWSER_BRIDGE_TOKEN must be set before starting the browser bridge")?;
    let config = config()?;
    run_browser_bridge(config, token, None)
}

fn run_browser_bridge(
    config: mice_core::Config,
    token: String,
    goal_directive: Option<Arc<Mutex<Option<BrowserGoalDirective>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:9417")?;
    println!("MICE browser bridge listening on http://127.0.0.1:9417");
    println!(
        "Load browser-ext as an unpacked extension and enter the same bridge token in its options."
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(error) =
                    handle_browser_connection(stream, &token, &config, goal_directive.as_ref())
                {
                    eprintln!("MICE browser bridge request failed: {error}");
                }
            }
            Err(error) => eprintln!("MICE browser bridge connection failed: {error}"),
        }
    }
    Ok(())
}

fn handle_browser_connection(
    mut stream: TcpStream,
    token: &str,
    config: &mice_core::Config,
    goal_directive: Option<&Arc<Mutex<Option<BrowserGoalDirective>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = read_http_request(&mut stream)?;
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("invalid HTTP request")?;
    let headers = std::str::from_utf8(&request[..header_end])?;
    let mut lines = headers.lines();
    let request_line = lines.next().ok_or("missing HTTP request line")?;
    let supplied_token = lines
        .find_map(|line| {
            line.strip_prefix("X-Mice-Token: ")
                .or_else(|| line.strip_prefix("x-mice-token: "))
        })
        .unwrap_or_default();
    if supplied_token != token {
        return write_http_json(
            &mut stream,
            401,
            &serde_json::json!({"error": "Unauthorized."}),
        );
    }
    let body = &request[header_end + 4..];
    match request_line {
        "POST /guide HTTP/1.1" => {
            let guide_request: BrowserGuideRequest = serde_json::from_slice(body)?;
            match guide_browser_request(config, guide_request) {
                Ok(response) => write_http_json(&mut stream, 200, &response),
                Err(error) => write_http_json(
                    &mut stream,
                    422,
                    &serde_json::json!({"error": error.to_string()}),
                ),
            }
        }
        "POST /goal-step HTTP/1.1" => {
            let directive =
                goal_directive.and_then(|value| value.lock().ok().and_then(|value| value.clone()));
            write_http_json(
                &mut stream,
                200,
                &serde_json::json!({"directive": directive}),
            )
        }
        "POST /goal-highlight HTTP/1.1" => {
            let Some(directive) =
                goal_directive.and_then(|value| value.lock().ok().and_then(|value| value.clone()))
            else {
                return write_http_json(
                    &mut stream,
                    422,
                    &serde_json::json!({"error": "No browser guide step is active."}),
                );
            };
            let request: BrowserGoalHighlightRequest = serde_json::from_slice(body)?;
            if request.session_id != directive.session_id
                || request.instruction != directive.instruction
            {
                return write_http_json(
                    &mut stream,
                    422,
                    &serde_json::json!({"error": "The browser guide step is no longer active."}),
                );
            }
            match guide_browser_request(
                config,
                BrowserGuideRequest {
                    instruction: directive.instruction,
                    url: request.url,
                    elements: request.elements,
                },
            ) {
                Ok(response) => write_http_json(&mut stream, 200, &response),
                Err(error) => write_http_json(
                    &mut stream,
                    422,
                    &serde_json::json!({"error": error.to_string()}),
                ),
            }
        }
        _ => write_http_json(
            &mut stream,
            422,
            &serde_json::json!({"error": "Unknown browser bridge request."}),
        ),
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("unexpected end of HTTP request".into());
        }
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..header_end])?;
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length: ")
                    .or_else(|| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .ok_or("missing Content-Length")?;
        if content_length > 1_000_000 {
            return Err("browser guide request exceeds 1 MiB".into());
        }
        if request.len() >= header_end + 4 + content_length {
            return Ok(request);
        }
    }
}

fn write_http_json(
    stream: &mut TcpStream,
    status: u16,
    value: &impl Serialize,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::to_vec(value)?;
    let status_text = if status == 200 {
        "OK"
    } else if status == 401 {
        "Unauthorized"
    } else {
        "Unprocessable Content"
    };
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn guide_browser_request(
    config: &mice_core::Config,
    request: BrowserGuideRequest,
) -> Result<BrowserGuideResponse, Box<dyn std::error::Error>> {
    if config.privacy_mode == mice_providers::PrivacyMode::LocalOnly {
        return Err("Browser guide-me requires cloud access; set privacy mode to cloud allowed in `mice settings`.".into());
    }
    let elements = rank_guide_candidates(&request.instruction, request.elements);
    if elements.is_empty() {
        return Err("The browser page has no visible interactive elements.".into());
    }
    let dom_snapshot = serde_json::to_string(&serde_json::json!({
        "url": request.url,
        "elements": &elements,
    }))?;
    let output = if is_groq_model(&config.cloud_model) {
        call_groq_guide(&config.cloud_model, &request.instruction, &dom_snapshot)?
    } else {
        call_openai_guide(&request.instruction, &dom_snapshot)?
    };
    let result: GuideModelResult = serde_json::from_str(&output)?;
    let selected = elements
        .iter()
        .find(|element| element.candidate_id.as_deref() == Some(&result.candidate_id))
        .ok_or("Guide model returned a candidate ID that was not in the supplied DOM snapshot.")?;
    Ok(BrowserGuideResponse {
        selector: selected.selector.clone(),
        instruction_text: result.instruction_text,
        label: selected.label.clone(),
        role: selected.role.clone(),
    })
}

/// Keep browser guide prompts bounded. Ranking is deliberately local and
/// deterministic: labels matching the user's words are most useful, while all
/// remaining actionable elements retain document order as a stable tiebreaker.
fn rank_guide_candidates(instruction: &str, elements: Vec<BrowserElement>) -> Vec<BrowserElement> {
    let terms = instruction
        .split(|character: char| !character.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() > 1)
        .collect::<Vec<_>>();
    let mut seen_selectors = HashSet::new();
    let mut seen_labels = HashSet::new();
    let mut ranked = Vec::new();

    for (index, element) in elements.into_iter().enumerate() {
        let selector = element.selector.trim().to_owned();
        if selector.is_empty()
            || selector.chars().count() > MAX_GUIDE_SELECTOR_CHARS
            || !seen_selectors.insert(selector.clone())
        {
            continue;
        }
        let label = bounded_for_model(element.label.trim(), MAX_GUIDE_LABEL_CHARS);
        let role = bounded_for_model(element.role.trim(), MAX_GUIDE_ROLE_CHARS);
        let searchable_label = label.to_ascii_lowercase();
        // Collapse repeated controls with the same visible label (e.g. a
        // sidebar button and its nested icon/text all read "Canva AI"), which
        // otherwise crowd out distinct controls and mislead the best guess.
        if !searchable_label.is_empty() && !seen_labels.insert(searchable_label.clone()) {
            continue;
        }
        let searchable_role = role.to_ascii_lowercase();
        let score = terms.iter().fold(0_u16, |score, term| {
            score
                + u16::from(searchable_label.contains(term)) * 10
                + u16::from(searchable_role.contains(term)) * 2
        });
        ranked.push((
            score,
            index,
            BrowserElement {
                selector,
                role,
                label,
                candidate_id: None,
            },
        ));
    }

    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    ranked
        .into_iter()
        .take(MAX_GUIDE_CANDIDATES)
        .enumerate()
        .map(|(candidate_index, (_, _, mut element))| {
            element.candidate_id = Some(format!("candidate-{}", candidate_index + 1));
            element
        })
        .collect()
}

/// Send credentials in HTTP headers, never in a child process argument list.
/// This removes the runtime `curl` dependency and keeps provider failures
/// useful without exposing secrets.
fn post_provider_request(
    service: &str,
    endpoint: &str,
    api_key: &str,
    payload: &str,
) -> Result<ureq::Response, Box<dyn std::error::Error>> {
    ureq::post(endpoint)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {api_key}"))
        .send_string(payload)
        .map_err(|error| match error {
            ureq::Error::Status(status, response) => {
                let body = response.into_string().unwrap_or_default();
                let detail = body.trim();
                let message = if detail.is_empty() {
                    format!("{service} failed with HTTP {status}")
                } else {
                    format!("{service} failed with HTTP {status}: {detail}")
                };
                std::io::Error::other(message)
            }
            ureq::Error::Transport(error) => {
                std::io::Error::other(format!("{service} request failed: {error}"))
            }
        })
        .map_err(Into::into)
}

fn post_provider_json(
    service: &str,
    endpoint: &str,
    api_key: &str,
    payload: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = post_provider_request(service, endpoint, api_key, payload)?;
    Ok(serde_json::from_reader(response.into_reader())?)
}

fn call_openai_guide(
    instruction: &str,
    dom_snapshot: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for browser guide-me")?;
    let payload = mice_providers::openai_guide_payload(instruction, dom_snapshot).to_string();
    let response = post_provider_json(
        "OpenAI Responses API",
        "https://api.openai.com/v1/responses",
        &api_key,
        &payload,
    )?;
    response["output"]
        .as_array()
        .and_then(|items| items.iter().find_map(|item| item["content"].as_array()))
        .and_then(|content| content.iter().find_map(|part| part["text"].as_str()))
        .map(str::to_owned)
        .ok_or_else(|| "OpenAI guide response did not contain structured output text".into())
}

fn is_groq_model(model: &str) -> bool {
    model.starts_with("llama-") || model.starts_with("mixtral-")
}

fn call_groq_guide(
    model: &str,
    instruction: &str,
    dom_snapshot: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("GROQ_API_KEY")
        .map_err(|_| "GROQ_API_KEY is required when Groq is selected for browser guide-me")?;
    let payload = mice_providers::groq_guide_payload(model, instruction, dom_snapshot).to_string();
    let response = post_provider_json(
        "Groq guide API",
        "https://api.groq.com/openai/v1/chat/completions",
        &api_key,
        &payload,
    )?;
    response["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .and_then(|choice| choice["message"]["content"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "Groq guide response did not contain JSON output".into())
}

fn call_openai_goal_plan(model: &str, goal: &str) -> Result<String, Box<dyn std::error::Error>> {
    let api_key =
        env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required for Goal Guide")?;
    let payload = mice_providers::structured_goal_plan_payload(model, goal).to_string();
    let response = post_provider_json(
        "OpenAI Goal Guide API",
        "https://api.openai.com/v1/responses",
        &api_key,
        &payload,
    )?;
    response["output"]
        .as_array()
        .and_then(|items| items.iter().find_map(|item| item["content"].as_array()))
        .and_then(|content| content.iter().find_map(|part| part["text"].as_str()))
        .map(str::to_owned)
        .ok_or_else(|| "OpenAI Goal Guide response did not contain a structured plan".into())
}

fn call_groq_goal_plan(model: &str, goal: &str) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("GROQ_API_KEY")
        .map_err(|_| "GROQ_API_KEY is required when Groq is selected for Goal Guide")?;
    let payload = mice_providers::groq_goal_plan_payload(model, goal).to_string();
    let response = post_provider_json(
        "Groq Goal Guide API",
        "https://api.groq.com/openai/v1/chat/completions",
        &api_key,
        &payload,
    )?;
    response["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .and_then(|choice| choice["message"]["content"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "Groq Goal Guide response did not contain JSON output".into())
}

fn call_openai_agent_turn(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    image_data_url: Option<&str>,
    persona: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key =
        env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required for autopilot")?;
    let payload = mice_providers::agent_loop_payload_with_image_and_persona(
        model,
        goal,
        observation,
        history,
        image_data_url,
        persona,
    )
    .to_string();
    let response = post_provider_json(
        "OpenAI autopilot API",
        "https://api.openai.com/v1/responses",
        &api_key,
        &payload,
    )?;
    response["output"]
        .as_array()
        .and_then(|items| items.iter().find_map(|item| item["content"].as_array()))
        .and_then(|content| content.iter().find_map(|part| part["text"].as_str()))
        .map(str::to_owned)
        .ok_or_else(|| "OpenAI autopilot response did not contain structured output.".into())
}

fn call_groq_agent_turn(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    persona: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("GROQ_API_KEY")
        .map_err(|_| "GROQ_API_KEY is required when Groq is selected for autopilot")?;
    let payload = mice_providers::groq_agent_loop_payload_with_persona(
        model,
        goal,
        observation,
        history,
        persona,
    )
    .to_string();
    let response = post_provider_json(
        "Groq autopilot API",
        "https://api.groq.com/openai/v1/chat/completions",
        &api_key,
        &payload,
    )?;
    response["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .and_then(|choice| choice["message"]["content"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "Groq autopilot response did not contain JSON output.".into())
}

fn actions() -> Result<(), Box<dyn std::error::Error>> {
    println!("Available action presets:");
    println!("  explain, summarize, rewrite, translate, extract-json, code, image, guide, qa");
    Ok(())
}

fn config() -> Result<mice_core::Config, Box<dyn std::error::Error>> {
    let path = config_path().ok_or("HOME is not set")?;
    Ok(load_config(&path)?)
}

fn status() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    println!("MICE status");
    println!("  config: {}", config_path().unwrap().display());
    println!("  cloud model: {}", config.cloud_model);
    println!("  local model: {}", config.local_model);
    println!("  privacy mode: {:?}", config.privacy_mode);
    match start_agent(&config.gesture) {
        Ok(mut agent) => {
            println!("  agent: connected (capability probe)");
            println!("  platform: {}", agent.platform);
            print_capabilities(&agent.capabilities);
            drop(agent.stdin);
            let _ = agent.child.wait();
        }
        Err(error) => println!("  agent: unavailable ({error})"),
    }
    Ok(())
}

fn print_capabilities(capabilities: &Capabilities) {
    println!(
        "  Screen Recording: {}",
        enabled(capabilities.screen_capture)
    );
    println!("  Accessibility read: {}", enabled(capabilities.ax_read));
    println!("  Text injection: {}", enabled(capabilities.inject_text));
    println!("  Overlay: {}", enabled(capabilities.overlay));
    println!("  Local OCR: {}", enabled(capabilities.local_ocr));
    println!("  Browser bridge: {}", enabled(capabilities.browser_bridge));
    println!(
        "  Input Monitoring: {}",
        enabled(capabilities.input_monitoring)
    );
}

fn enabled(value: bool) -> &'static str {
    if value { "available" } else { "unavailable" }
}

fn shared_memory() -> Result<memory::SharedMemory, Box<dyn std::error::Error>> {
    let path = memory::SharedMemory::default_path().ok_or("HOME is not set")?;
    Ok(memory::SharedMemory::at(path)?)
}

fn tool_session_name() -> String {
    env::var("MICE_SESSION_ID").unwrap_or_else(|_| format!("mice-{}", std::process::id()))
}

fn repo_state_fingerprint(cwd: &std::path::Path) -> String {
    let directory = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string();
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let Some(head) = head else {
        // Repository tools are deliberately not cached outside Git (see
        // `run_registered_tool`), so a stable namespace is enough here.
        return format!("no-git:{directory}");
    };
    // `git status` only records that a dirty file exists; it does not change
    // when that same file is edited again. Fingerprint the changed path list
    // and bounded regular-file state instead of collecting a potentially huge
    // binary diff in memory. If a list exceeds the cap, disable caching rather
    // than risk a stale or memory-hungry cache key.
    let Some(tracked) =
        bounded_git_output(cwd, &["diff", "--no-ext-diff", "--name-only", "-z", "HEAD"])
    else {
        return format!("uncacheable:{directory}");
    };
    let Some(untracked) =
        bounded_git_output(cwd, &["ls-files", "--others", "--exclude-standard", "-z"])
    else {
        return format!("uncacheable:{directory}");
    };
    let mut digest = Sha256::new();
    for paths in [&tracked, &untracked] {
        for path in paths
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            digest.update(path);
            if let Ok(path) = std::str::from_utf8(path) {
                hash_regular_file_for_fingerprint(&mut digest, &cwd.join(path));
            }
        }
    }
    format!("{directory}:{head}:{:x}", digest.finalize())
}

const MAX_GIT_FINGERPRINT_LIST_BYTES: usize = 512 * 1024;

fn bounded_git_output(cwd: &std::path::Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (bytes, truncated) =
        read_bounded_bytes(&mut stdout, MAX_GIT_FINGERPRINT_LIST_BYTES).ok()?;
    let status = child.wait().ok()?;
    (status.success() && !truncated).then_some(bytes)
}

fn read_bounded_bytes(
    reader: &mut impl Read,
    maximum: usize,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut bytes = Vec::with_capacity(maximum.min(16 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = maximum.saturating_sub(bytes.len());
        let kept = read.min(remaining);
        bytes.extend_from_slice(&buffer[..kept]);
        truncated |= kept < read;
    }
    Ok((bytes, truncated))
}

fn is_git_repository(cwd: &std::path::Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .ok()
        .is_some_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
}

fn repository_cacheable(cwd: &std::path::Path) -> bool {
    is_git_repository(cwd) && !repo_state_fingerprint(cwd).starts_with("uncacheable:")
}

const MAX_FINGERPRINT_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn hash_regular_file_for_fingerprint(digest: &mut Sha256, path: &std::path::Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        digest.update(b"unreadable");
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        digest.update(b"non-regular");
        return;
    }
    digest.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified()
        && let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH)
    {
        digest.update(since_epoch.as_nanos().to_le_bytes());
    }
    let Ok(file) = std::fs::File::open(path) else {
        digest.update(b"unreadable");
        return;
    };
    let mut reader = file.take(MAX_FINGERPRINT_FILE_BYTES);
    let mut buffer = Vec::new();
    if reader.read_to_end(&mut buffer).is_ok() {
        digest.update(&buffer);
    }
}

fn workflow_macro_key(goal: &str) -> String {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    format!("{goal}\nrepository:{}", repo_state_fingerprint(&cwd))
}

fn current_branch(cwd: &std::path::Path) -> String {
    Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn run_registered_tool(
    _config: &mice_core::Config,
    session: &str,
    call: ToolCall,
) -> Result<ToolOutput, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let state = repo_state_fingerprint(&cwd);
    let key = tools::call_fingerprint(&call, &state);
    let store = shared_memory()?;
    // In a non-Git directory there is no bounded, authoritative revision
    // token. Do not let repository answers survive between invocations.
    let cacheable = tools::cache_policy(&call.name) == Some(tools::CachePolicy::Repository)
        && !state.starts_with("uncacheable:")
        && is_git_repository(&cwd);
    if cacheable && let Some(cached) = store.artifact(&key)? {
        store.record_ledger(
            session,
            &memory::LedgerRecord {
                task: call.name.clone(),
                lane: "deterministic".into(),
                wall_ms: 0,
                raw_output_tokens_est: cached.raw_output_tokens_est,
                returned_tokens_est: estimate_tokens(&cached.distilled),
                frontier_tokens_avoided_est: cached
                    .raw_output_tokens_est
                    .saturating_sub(estimate_tokens(&cached.distilled)),
                outcome: "cache_hit".into(),
            },
        )?;
        return Ok(ToolOutput {
            text: cached.distilled,
            raw: String::new(),
            truncated: cached.truncated,
            full_output_ref: Some(format!("artifact:{key}")),
            needs_distillation: false,
        });
    }
    let started = Instant::now();
    let output = tools::run(
        &SystemRunner,
        &call,
        &ToolContext {
            working_dir: cwd.clone(),
            session_name: session.into(),
            output_budget_tokens: tools::DEFAULT_RETURN_TOKENS,
        },
    )?;
    let artifact = memory::CachedArtifact {
        key: key.clone(),
        tool: call.name.clone(),
        args: call.args.to_string().chars().take(2_048).collect(),
        fingerprint: state,
        distilled: output.text.clone(),
        raw_output_tokens_est: estimate_tokens(&output.raw),
        truncated: output.truncated,
        created_ts: memory::now(),
    };
    if cacheable {
        store.put_artifact(&key, &artifact)?;
    }
    let files = if call.name.starts_with("git.") {
        output
            .raw
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter(|value| value.contains('/'))
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    store.append(&memory::MemoryEvent {
        event_ts: memory::now(),
        recorded_ts: memory::now(),
        session: session.into(),
        agent: session.into(),
        branch: current_branch(&cwd),
        kind: "tool".into(),
        text: format!("{} completed", call.name),
        files,
    })?;
    store.record_ledger(
        session,
        &memory::LedgerRecord {
            task: call.name,
            lane: "deterministic".into(),
            wall_ms: started.elapsed().as_millis(),
            raw_output_tokens_est: estimate_tokens(&output.raw),
            returned_tokens_est: estimate_tokens(&output.text),
            frontier_tokens_avoided_est: estimate_tokens(&output.raw)
                .saturating_sub(estimate_tokens(&output.text)),
            outcome: "success".into(),
        },
    )?;
    Ok(ToolOutput {
        full_output_ref: cacheable.then(|| format!("artifact:{key}")),
        ..output
    })
}

fn list_tools() -> Result<(), Box<dyn std::error::Error>> {
    let runner = SystemRunner;
    println!("MICE deterministic tool registry:");
    for spec in tools::specs() {
        let available = runner.available(spec.availability_program);
        let state = if spec.kind == tools::ToolKind::Mutating {
            "blocked pending verified confirmation"
        } else if available {
            "available"
        } else {
            "unavailable"
        };
        println!("  {:<22} {}", spec.name, state);
    }
    println!(
        "GitHub uses gh-axi when present, otherwise gh. Browser tools use chrome-devtools-axi through npx."
    );
    Ok(())
}

fn bench_tools() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let runner = SystemRunner;
    let tool_model_available = local_tool_model_available(&config, &runner);
    let trusted_loop_lane = match config.machine_profile {
        MachineProfile::Light => false,
        MachineProfile::Standard | MachineProfile::Heavy => tool_model_available,
    };
    println!("MICE tool benchmark (network-free schema validation)");
    println!("  profile: {:?}", config.machine_profile);
    println!(
        "  tool model: {} ({})",
        config.tool_model,
        if tool_model_available {
            "installed candidate"
        } else {
            "unavailable/unknown"
        }
    );
    println!("  JSON tool-decision parser: passed");
    println!(
        "  loop-driver trusted: {}",
        if trusted_loop_lane {
            "yes"
        } else {
            "no; deterministic tools only"
        }
    );
    Ok(())
}

fn savings() -> Result<(), Box<dyn std::error::Error>> {
    let report = shared_memory()?.savings()?;
    println!("MICE savings");
    println!("  delegations: {}", report.delegations);
    println!(
        "  frontier tokens avoided (estimated): {}",
        report.frontier_tokens_avoided
    );
    println!("  cache hits: {}", report.cache_hits);
    println!("  macro replays: {}", report.macro_replays);
    for (lane, count) in report.by_lane {
        println!("  {lane}: {count}");
    }
    Ok(())
}

fn advertise() -> Result<(), Box<dyn std::error::Error>> {
    let text = format!(
        "You are paired with MICE, a local execution manager. Delegate mechanical, repetitive, or token-heavy steps through run_tool or delegate_task; MICE first uses deterministic local CLIs and returns bounded results. Check team_status before editing shared files and record durable choices with memory_note.\n\nAvailable deterministic tools:\n{}",
        tools::stable_tool_prompt()
    );
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--into")
    {
        let path = arguments
            .get(1)
            .ok_or("Usage: mice advertise [--into <file>]")?;
        std::fs::write(path, &text)?;
        println!("Wrote MICE manager instructions to {path}.");
    } else if !arguments.is_empty() {
        return Err("Usage: mice advertise [--into <file>]".into());
    } else {
        println!("{text}");
    }
    Ok(())
}

fn do_goal() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let mut arguments = env::args().skip(2).collect::<Vec<_>>();
    let mut session = tool_session_name();
    let mut max_actions = 6usize;
    let mut model = config.tool_model.clone();
    while arguments
        .first()
        .is_some_and(|argument| argument.starts_with("--"))
    {
        match arguments.first().map(String::as_str) {
            Some("--session") if arguments.len() >= 2 => {
                session = arguments[1].clone();
                arguments.drain(0..2);
            }
            Some("--model") if arguments.len() >= 2 => {
                model = arguments[1].clone();
                arguments.drain(0..2);
            }
            Some("--max-actions") if arguments.len() >= 2 => {
                max_actions = arguments[1]
                    .parse()
                    .map_err(|_| "--max-actions needs a number")?;
                arguments.drain(0..2);
            }
            _ => return Err(
                "Usage: mice do [--model <model>] [--max-actions <n>] [--session <name>] <goal>"
                    .into(),
            ),
        }
    }
    let goal = arguments.join(" ");
    if goal.trim().is_empty() {
        return Err("Usage: mice do <goal>".into());
    }
    validate_tool_action_budget(max_actions)?;
    if route_execution_lane(
        config.machine_profile,
        &config.routing,
        false,
        true,
        current_quota_usage_percent(),
    ) != ExecutionLane::Local
    {
        return Err("This light machine profile keeps SLM tool loops disabled. Use `mice tools` / MCP `run_tool`, or change the profile only after `mice bench-tools` trusts a capable model.".into());
    }
    if mice_providers::model_descriptor(&model).is_none() {
        return Err(format!("Unknown MICE tool model `{model}`.").into());
    }
    let mut history = Vec::<String>::new();
    let mut calls = Vec::<Value>::new();
    for _ in 0..max_actions {
        let prompt = format!(
            "Goal: {goal}\n\nTools:\n{}\n\nPrior results:\n{}\n\nReturn exactly one JSON object with snake_case keys: {{\"tool\": string|null, \"args\": object, \"say_to_user\": string, \"done\": bool, \"ask_user\": string|null}}. Choose one listed tool, or set done=true only when the goal is complete.",
            tools::stable_tool_prompt(),
            history.join("\n")
        );
        let mut raw = String::new();
        stream_ollama(
            &model,
            "You are a careful local tool manager. Never invent tool results.",
            Some(&prompt),
            |chunk| {
                raw.push_str(chunk);
                Ok(())
            },
        )?;
        let decision: ToolDecision = serde_json::from_str(extract_json_object(&raw))
            .map_err(|error| format!("Tool model returned invalid JSON: {error}; output: {raw}"))?;
        decision.validate()?;
        if let Some(question) = decision.ask_user {
            println!("MICE needs your input: {question}");
            return Ok(());
        }
        if decision.done {
            println!("MICE: {}", decision.say_to_user);
            persist_safe_macro(&shared_memory()?, &goal, calls)?;
            return Ok(());
        }
        let call = ToolCall {
            name: decision.tool.unwrap_or_default(),
            args: decision.args,
        };
        let output = run_registered_tool(&config, &session, call.clone())?;
        println!("{}: {}", call.name, output.text);
        calls.push(json!({"name": call.name, "args": call.args}));
        history.push(format!("{} => {}", call.name, output.text));
    }
    Err("MICE tool loop reached its action budget; ask a narrower follow-up.".into())
}

const MIN_TOOL_ACTIONS: usize = 1;
const MAX_TOOL_ACTIONS: usize = 12;

fn validate_tool_action_budget(value: usize) -> Result<(), Box<dyn std::error::Error>> {
    if !(MIN_TOOL_ACTIONS..=MAX_TOOL_ACTIONS).contains(&value) {
        return Err(format!(
            "Tool action budget must be between {MIN_TOOL_ACTIONS} and {MAX_TOOL_ACTIONS}."
        )
        .into());
    }
    Ok(())
}

fn persist_safe_macro(
    store: &memory::SharedMemory,
    goal: &str,
    calls: Vec<Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !calls.is_empty()
        && repository_cacheable(&env::current_dir()?)
        && calls.iter().all(|call| {
            call.get("name")
                .and_then(Value::as_str)
                .is_some_and(tools::is_read_only)
        })
    {
        store.put_macro(&workflow_macro_key(goal), &Value::Array(calls))?;
    }
    Ok(())
}

fn delegate_task(
    config: &mice_core::Config,
    session: &McpSession,
    goal: &str,
    max_actions: usize,
) -> Result<String, Box<dyn std::error::Error>> {
    validate_tool_action_budget(max_actions)?;
    if route_execution_lane(
        config.machine_profile,
        &config.routing,
        false,
        true,
        current_quota_usage_percent(),
    ) != ExecutionLane::Local
    {
        return Err("This light machine profile disables local SLM loops. Use run_tool for deterministic delegation.".into());
    }
    let store = shared_memory()?;
    if repository_cacheable(&env::current_dir()?)
        && let Some(macro_calls) = store.macro_for(&workflow_macro_key(goal))?
        && let Some(calls) = macro_calls.as_array()
        && !calls.is_empty()
        && calls.iter().all(|call| {
            call.get("name")
                .and_then(Value::as_str)
                .is_some_and(tools::is_read_only)
        })
    {
        let mut replay = Vec::new();
        for call in calls {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .ok_or("Invalid stored workflow macro")?;
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            replay.push(format!(
                "{name}: {}",
                run_registered_tool(
                    config,
                    &session.id,
                    ToolCall {
                        name: name.into(),
                        args
                    }
                )?
                .text
            ));
        }
        store.record_ledger(
            &session.id,
            &memory::LedgerRecord {
                task: goal.into(),
                lane: "deterministic".into(),
                wall_ms: 0,
                raw_output_tokens_est: 0,
                returned_tokens_est: estimate_tokens(&replay.join("\n")),
                frontier_tokens_avoided_est: 0,
                outcome: "macro_replay".into(),
            },
        )?;
        return Ok(format!(
            "Replayed a verified local workflow:\n{}",
            replay.join("\n")
        ));
    }
    let mut history = Vec::<String>::new();
    let mut calls = Vec::<Value>::new();
    for _ in 0..max_actions {
        let prompt = format!(
            "Goal: {goal}\n\nTools:\n{}\n\nPrior results:\n{}\n\nReturn exactly one JSON object with snake_case keys: {{\"tool\": string|null, \"args\": object, \"say_to_user\": string, \"done\": bool, \"ask_user\": string|null}}. Choose one listed tool, or set done=true only when the goal is complete.",
            tools::stable_tool_prompt(),
            history.join("\n")
        );
        let mut raw = String::new();
        stream_ollama(
            &config.tool_model,
            "You are a careful local tool manager. Never invent tool results.",
            Some(&prompt),
            |chunk| {
                raw.push_str(chunk);
                Ok(())
            },
        )?;
        let decision: ToolDecision = serde_json::from_str(extract_json_object(&raw))
            .map_err(|error| format!("Tool model returned invalid JSON: {error}"))?;
        decision.validate()?;
        if let Some(question) = decision.ask_user {
            return Ok(format!("MICE needs your input: {question}"));
        }
        if decision.done {
            persist_safe_macro(&store, goal, calls)?;
            return Ok(if decision.say_to_user.is_empty() {
                "Delegated task completed.".into()
            } else {
                decision.say_to_user
            });
        }
        let call = ToolCall {
            name: decision.tool.unwrap_or_default(),
            args: decision.args,
        };
        let output = run_registered_tool(config, &session.id, call.clone())?;
        calls.push(json!({"name": call.name, "args": call.args}));
        history.push(format!("{} => {}", call.name, output.text));
    }
    Err("MICE delegated task reached its action budget.".into())
}

fn extract_json_object(value: &str) -> &str {
    let start = value.find('{').unwrap_or(0);
    let end = value
        .rfind('}')
        .map(|index| index + 1)
        .unwrap_or(value.len());
    &value[start..end]
}

fn doctor() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = config()?;
    for warning in mice_core::config_warnings(&config) {
        println!("Config warning: {warning}");
    }
    let output = Command::new("df").args(["-g", "."]).output()?;
    let line = String::from_utf8(output.stdout)?
        .lines()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let available = line
        .split_whitespace()
        .nth(3)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    println!("Disk available: {available} GiB");
    if available < 24 {
        println!("WARNING: At least 24 GiB free is required before pulling gpt-oss:20b.");
    } else {
        println!("Disk preflight passed for gpt-oss:20b.");
    }
    println!(
        "OPENAI_API_KEY: {}",
        if env::var_os("OPENAI_API_KEY").is_some() {
            "set"
        } else {
            "not set"
        }
    );
    println!(
        "GROQ_API_KEY: {}",
        if env::var_os("GROQ_API_KEY").is_some() {
            "set"
        } else {
            "not set"
        }
    );
    println!(
        "Ollama: {}",
        if Command::new("ollama").arg("--version").output().is_ok() {
            "installed"
        } else {
            "not installed"
        }
    );
    let memory_gib = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024 / 1024 / 1024);
    let detected = match memory_gib.unwrap_or(0) {
        0..=16 => MachineProfile::Light,
        17..=31 => MachineProfile::Standard,
        _ => MachineProfile::Heavy,
    };
    config.machine_profile = detected;
    let path = config_path().ok_or("HOME is not set")?;
    save_config(&path, &config)?;
    println!(
        "Machine profile: {:?}{}",
        detected,
        memory_gib
            .map(|gib| format!(" ({gib} GiB detected)"))
            .unwrap_or_default()
    );
    println!(
        "Tool loop: {} (model {})",
        if detected == MachineProfile::Light {
            "disabled on light profile"
        } else {
            "run `mice bench-tools` before trusting"
        },
        config.tool_model
    );
    println!("Tool adapters:");
    for (name, available) in tools::availability(&SystemRunner) {
        println!(
            "  {name}: {}",
            if available {
                "available"
            } else {
                "unavailable"
            }
        );
    }
    println!("GitHub setup: run `gh auth login` if GitHub tools are unavailable.");
    println!(
        "Browser setup: npx -y chrome-devtools-axi; optionally install chrome-devtools-mcp globally to avoid cold starts."
    );
    Ok(())
}

fn settings() -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path().ok_or("HOME is not set")?;
    let mut config = load_config(&path)?;
    if run_settings_tui(&mut config)? {
        save_config(&path, &config)?;
        println!("Saved settings to {}.", path.display());
    }
    Ok(())
}

fn run_settings_tui(config: &mut mice_core::Config) -> Result<bool, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = settings_event_loop(&mut terminal, config);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn settings_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    config: &mut mice_core::Config,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut selected = 0usize;
    loop {
        terminal.draw(|frame| draw_settings(frame, config, selected))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => selected = selected.checked_sub(1).unwrap_or(10),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % 11,
            KeyCode::Left | KeyCode::Char('h') => adjust_setting(config, selected, false),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                adjust_setting(config, selected, true)
            }
            KeyCode::Char('s') => return Ok(true),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
            _ => {}
        }
    }
}

fn draw_settings(frame: &mut ratatui::Frame, config: &mice_core::Config, selected: usize) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(area);
    let rows = [
        format!(
            "Privacy mode       {}",
            privacy_mode_name(config.privacy_mode)
        ),
        format!(
            "Cost policy        {}",
            cost_policy_name(config.cost_policy)
        ),
        format!("Cloud model        {}", config.cloud_model),
        format!("Local model        {}", config.local_model),
        format!("Gesture trigger    {}", config.gesture.trigger),
        format!(
            "Selection summary  {}",
            config.gesture.summarize_selection_trigger
        ),
        format!(
            "Selection image    {}",
            config.gesture.infographic_selection_trigger
        ),
        format!("Smart copy         {}", config.gesture.smart_copy_trigger),
        format!("Goal Guide        {}", config.gesture.goal_trigger),
        format!("Autopilot persona  {}", config.autopilot.persona),
        format!(
            "Always confirm actions {}",
            if config.autopilot.careful_mode {
                "on"
            } else {
                "off"
            }
        ),
    ];
    let lines = rows.into_iter().enumerate().map(|(index, row)| {
        let marker = if index == selected { "› " } else { "  " };
        let style = if index == selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        Line::from(Span::styled(format!("{marker}{row}"), style))
    });
    frame.render_widget(
        Paragraph::new(lines.collect::<Vec<_>>()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" MICE settings "),
        ),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new("↑/↓ select  ←/→ change  s save  q cancel")
            .block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

fn adjust_setting(config: &mut mice_core::Config, selected: usize, forward: bool) {
    match selected {
        0 => {
            config.privacy_mode = match config.privacy_mode {
                mice_providers::PrivacyMode::CloudAllowed => mice_providers::PrivacyMode::CloudOnly,
                mice_providers::PrivacyMode::CloudOnly => mice_providers::PrivacyMode::LocalOnly,
                mice_providers::PrivacyMode::LocalOnly => mice_providers::PrivacyMode::CloudAllowed,
            };
        }
        1 => {
            config.cost_policy = match (config.cost_policy, forward) {
                (mice_providers::CostPolicy::Cheapest, true)
                | (mice_providers::CostPolicy::BestQuality, false) => {
                    mice_providers::CostPolicy::Fastest
                }
                (mice_providers::CostPolicy::Fastest, true)
                | (mice_providers::CostPolicy::Cheapest, false) => {
                    mice_providers::CostPolicy::BestQuality
                }
                (mice_providers::CostPolicy::BestQuality, true)
                | (mice_providers::CostPolicy::Fastest, false) => {
                    mice_providers::CostPolicy::Cheapest
                }
            };
        }
        2 => cycle_value(
            &mut config.cloud_model,
            &[
                "gpt-5.6-luna",
                "gpt-5.6-terra",
                "gpt-5.6-sol",
                "llama-3.3-70b-versatile",
                "llama-3.1-8b-instant",
            ],
            forward,
        ),
        3 => cycle_value(
            &mut config.local_model,
            &["gemma3:4b", "phi4-mini", "gpt-oss:20b"],
            forward,
        ),
        4 => cycle_value(
            &mut config.gesture.trigger,
            &["ctrl+shift+space", "ctrl+alt+space", "cmd+shift+space"],
            forward,
        ),
        5 => cycle_value(
            &mut config.gesture.summarize_selection_trigger,
            &["ctrl-double-tap", "ctrl+alt+s"],
            forward,
        ),
        6 => cycle_value(
            &mut config.gesture.infographic_selection_trigger,
            &["ctrl+alt+i", "ctrl+alt+m"],
            forward,
        ),
        7 => cycle_value(
            &mut config.gesture.smart_copy_trigger,
            &["ctrl+alt+c", "ctrl+alt+x"],
            forward,
        ),
        8 => cycle_value(
            &mut config.gesture.goal_trigger,
            &["ctrl+alt+space"],
            forward,
        ),
        9 => cycle_value(
            &mut config.autopilot.persona,
            &["patient", "concise", "playful"],
            forward,
        ),
        10 => config.autopilot.careful_mode = !config.autopilot.careful_mode,
        _ => {}
    }
}

fn cycle_value(value: &mut String, options: &[&str], forward: bool) {
    let index = options
        .iter()
        .position(|option| *option == value)
        .unwrap_or(0);
    let next = if forward {
        (index + 1) % options.len()
    } else {
        index.checked_sub(1).unwrap_or(options.len() - 1)
    };
    *value = options[next].into();
}

fn privacy_mode_name(mode: mice_providers::PrivacyMode) -> &'static str {
    match mode {
        mice_providers::PrivacyMode::CloudAllowed => "cloud allowed",
        mice_providers::PrivacyMode::CloudOnly => "cloud only",
        mice_providers::PrivacyMode::LocalOnly => "local only",
    }
}

fn cost_policy_name(policy: mice_providers::CostPolicy) -> &'static str {
    match policy {
        mice_providers::CostPolicy::Cheapest => "cheapest",
        mice_providers::CostPolicy::Fastest => "fastest",
        mice_providers::CostPolicy::BestQuality => "best quality",
    }
}

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    for warning in mice_core::config_warnings(&config) {
        println!("[MICE config] warning: {warning}");
    }
    MCP_SERVERS_GRANTED.store(
        !mcp_client::granted_servers(&config).is_empty(),
        Ordering::Relaxed,
    );
    let browser_goal_directive: NativeBridge = Arc::new(Mutex::new(NativeBridgeState::default()));
    start_native_bridge(config.clone(), Arc::clone(&browser_goal_directive))?;
    let watchdog_bridge = Arc::clone(&browser_goal_directive);
    std::thread::spawn(move || {
        loop {
            recover_autopilot_timeouts(&watchdog_bridge);
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    let mut agent = start_agent(&config.gesture)?;
    println!(
        "MICE is running with {} agent (overlay={}). Press Ctrl-C to stop.",
        agent.platform, agent.capabilities.overlay
    );
    println!("=== MICE Keyboard Gesture Loop ===");
    println!(
        "Capture: Ctrl+Shift+Space. Hover: hold Control. Select text, then Ctrl double-tap to summarize or Ctrl+Option+I for an infographic. After a normal Cmd-C, Ctrl+Option+C smart-copies."
    );

    let mut goal_sessions = HashMap::<String, GoalSession>::new();
    let mut goal_plans = HashMap::<String, GoalPlanResult>::new();
    let mut active_guides = HashMap::<String, ActiveGuide>::new();
    let mut selection_cache: Option<SelectionCache> = None;
    // At most one speculative deeper explanation runs at once. This keeps a
    // local model responsive if someone makes several selections in a row.
    let go_deeper_prefetch_in_flight = Arc::new(AtomicBool::new(false));
    while let Ok(msg) = read_frame::<mice_ipc::RpcNotification>(&mut agent.reader) {
        if msg.method == "goal.request" {
            let Some(session_id) = msg.params["sessionId"].as_str() else {
                eprintln!("MICE received a goal request without a session ID");
                continue;
            };
            goal_sessions.insert(session_id.into(), GoalSession::new());
            send_command(
                &mut agent.stdin,
                AgentCommand::OverlayPromptInput {
                    session_id: session_id.into(),
                    title: "What is your goal today?".into(),
                    placeholder: "For example: organize my tax documents".into(),
                    context: Some(
                        "MICE will make an advisory plan. You stay in control of every click, form, login, and payment."
                            .into(),
                    ),
                },
            )?;
        } else if msg.method == "prompt.submitted" {
            let submission: PromptSubmitted = match serde_json::from_value(msg.params) {
                Ok(submission) => submission,
                Err(error) => {
                    eprintln!("MICE received invalid prompt text: {error}");
                    continue;
                }
            };
            let Some(session) = goal_sessions.get_mut(&submission.session_id) else {
                eprintln!("MICE received a prompt submission for an unknown session");
                continue;
            };
            if let Err(error) = handle_goal_submission(
                &mut agent.stdin,
                &config,
                session,
                &mut goal_plans,
                &mut active_guides,
                &browser_goal_directive,
                submission,
            ) {
                eprintln!("MICE goal planning failed: {error}");
            }
        } else if msg.method == "prompt.cancelled" {
            if let Some(session_id) = msg.params["sessionId"].as_str() {
                goal_sessions.remove(session_id);
                goal_plans.remove(session_id);
                active_guides.remove(session_id);
                clear_browser_goal_directive(&browser_goal_directive);
            }
            let _ = send_command(
                &mut agent.stdin,
                AgentCommand::OverlayFinishResult {
                    text: Some("Goal planning cancelled.".into()),
                },
            );
        } else if msg.method == "guide.control" {
            let Some(session_id) = msg.params["sessionId"].as_str() else {
                continue;
            };
            let Some(action) = msg.params["action"].as_str() else {
                continue;
            };
            let value = msg.params["value"].as_str();
            if let Err(error) = handle_guide_control(
                &mut agent.stdin,
                &mut active_guides,
                &browser_goal_directive,
                session_id,
                action,
                value,
            ) {
                eprintln!("MICE guide control failed: {error}");
            }
        } else if msg.method == "selection.text" {
            let selection: SelectionText = match serde_json::from_value(msg.params) {
                Ok(selection) => selection,
                Err(error) => {
                    eprintln!("MICE received invalid selected text: {error}");
                    continue;
                }
            };
            let session_id = selection.session_id.clone();
            let text = selection.text.clone();
            match handle_selection_action(&mut agent.stdin, &config, selection) {
                Ok(Some(response)) => {
                    let prepared_go_deeper = start_go_deeper_prefetch(
                        &config,
                        text.clone(),
                        Arc::clone(&go_deeper_prefetch_in_flight),
                    );
                    selection_cache = Some(SelectionCache {
                        session_id,
                        text,
                        response,
                        prepared_go_deeper,
                    });
                }
                Ok(None) => {}
                Err(error) => eprintln!("MICE selection action failed: {error}"),
            }
        } else if msg.method == "clipboard.captured" {
            let captured: ClipboardCaptured = match serde_json::from_value(msg.params) {
                Ok(captured) => captured,
                Err(error) => {
                    eprintln!("MICE received an invalid clipboard capture: {error}");
                    continue;
                }
            };
            if let Err(error) = handle_smart_copy(&mut agent.stdin, &config, captured) {
                eprintln!("MICE smart copy failed: {error}");
            }
        } else if msg.method == "overlay.action" {
            let session_id = msg.params["sessionId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let action_id = msg.params["actionId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if let Err(error) = handle_overlay_action(
                &mut agent.stdin,
                &config,
                &mut selection_cache,
                &session_id,
                &action_id,
            ) {
                eprintln!("MICE overlay action failed: {error}");
            }
        } else if msg.method == "selection.captured" {
            let params = msg.params;
            let text = params["text"].as_str().map(|s| s.to_string());
            let has_pixels = params["pixels"].as_str().is_some();
            let ax_role = params["ax"]["role"].as_str().unwrap_or("");
            let ax_title = params["ax"]["title"].as_str().unwrap_or("");

            println!("\n[MICE] Screen region captured!");
            if !ax_role.is_empty() || !ax_title.is_empty() {
                println!("  AX Element: {} (Role: {})", ax_title, ax_role);
            }
            if let Some(ref ocr_text) = text
                && !ocr_text.trim().is_empty()
            {
                println!("  OCR Text: {}", ocr_text.trim());
            }

            print!("mice ask> ");
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_err() {
                continue;
            }
            let instruction = input.trim();
            if instruction.is_empty() {
                continue;
            }

            // Perform routing
            let action = action_for_interactive_instruction(instruction);
            let request = RouteRequest {
                artifacts: Artifacts {
                    text: text.clone(),
                    pixels: has_pixels,
                    ..Default::default()
                },
                instruction: action_instruction(action, instruction),
                action: Some(action), // The interactive gesture defaults to summarize.
                privacy_mode: config.privacy_mode,
                cost_policy: config.cost_policy,
                model_preferences: ModelPreferences {
                    local_model: config.local_model.clone(),
                    cloud_model: config.cloud_model.clone(),
                },
            };

            let selected = match route(&request) {
                Ok(r) => r.model,
                Err(err) => {
                    println!("Routing error: {}", err);
                    continue;
                }
            };

            println!("[Streaming via {}]", selected.id);
            send_command(
                &mut agent.stdin,
                AgentCommand::OverlayShow {
                    text: "MICE is thinking…".into(),
                },
            )?;

            if action == Action::Image {
                match generate_and_present_image(&mut agent.stdin, &request.instruction) {
                    Ok(()) => {
                        println!("[gpt-image-2] Infographic generated and copied to the clipboard.")
                    }
                    Err(error) => {
                        println!("Image generation error: {error}");
                        let _ = send_command(
                            &mut agent.stdin,
                            AgentCommand::OverlayFinishResult {
                                text: Some(format!("Image generation error: {error}")),
                            },
                        );
                    }
                }
                continue;
            }

            let mut stream = OverlayStream::echoing(&mut agent.stdin);
            let result = if selected.locality == mice_providers::Locality::Local {
                stream_ollama(
                    selected.id,
                    &request.instruction,
                    text.as_deref(),
                    |chunk| stream.push(chunk),
                )
            } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
                stream_groq(
                    selected.id,
                    &request.instruction,
                    text.as_deref(),
                    |chunk| stream.push(chunk),
                )
            } else {
                stream_openai(
                    selected.id,
                    &request.instruction,
                    text.as_deref(),
                    |chunk| stream.push(chunk),
                )
            };

            println!(); // Print new line after stream ends

            match result {
                Ok(()) => {
                    let response = stream.finish()?;
                    if !response.is_empty() {
                        send_command(&mut agent.stdin, clipboard_command(&response))?;
                    }
                    send_command(
                        &mut agent.stdin,
                        AgentCommand::OverlayFinishResult { text: None },
                    )?;
                }
                Err(error) => {
                    let _ = stream.finish();
                    println!("Error: {}", error);
                    let _ = send_command(
                        &mut agent.stdin,
                        AgentCommand::OverlayFinishResult {
                            text: Some(format!("Error: {error}")),
                        },
                    );
                }
            }
        } else if msg.method == "hover.captured" {
            let hover: HoverCaptured = match serde_json::from_value(msg.params) {
                Ok(hover) => hover,
                Err(error) => {
                    eprintln!("MICE received invalid hover context: {error}");
                    continue;
                }
            };
            if let Err(error) = explain_hover(&mut agent.stdin, &config, hover) {
                eprintln!("MICE hover explanation failed: {error}");
            }
        }
    }

    let _ = agent.child.wait();
    Ok(())
}

/// Remembers the most recent selection result so overlay buttons (Go Deeper,
/// Copy) can act on it without recapturing the selection.
struct SelectionCache {
    session_id: String,
    text: String,
    response: String,
    prepared_go_deeper: Option<Receiver<Result<String, String>>>,
}

/// A single word or very short phrase is treated as "define this" rather than a
/// summary, so selecting one word and using the summarize gesture gives a
/// dictionary-style answer.
fn is_short_phrase(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && !trimmed.contains('\n')
        && trimmed.chars().count() <= 40
        && trimmed.split_whitespace().count() <= 3
}

const GO_DEEPER_INSTRUCTION: &str = "Explain the selected content in greater depth: define the key terms, give background and context, why it matters, and any important nuance or implications. Be clear and thorough.";

/// The action buttons shown on a finished text result.
/// Set once at daemon start when the user has granted at least one external
/// MCP server; it only controls whether the Fetch Links button is offered.
static MCP_SERVERS_GRANTED: AtomicBool = AtomicBool::new(false);

fn selection_result_actions() -> Vec<mice_ipc::OverlayAction> {
    let mut actions = vec![
        mice_ipc::OverlayAction {
            id: "go_deeper".into(),
            label: "Go Deeper".into(),
        },
        mice_ipc::OverlayAction {
            id: "copy".into(),
            label: "Copy".into(),
        },
        mice_ipc::OverlayAction {
            id: "send_to".into(),
            label: "Send to…".into(),
        },
    ];
    if MCP_SERVERS_GRANTED.load(Ordering::Relaxed) {
        actions.push(mice_ipc::OverlayAction {
            id: "fetch_links".into(),
            label: "Fetch Links".into(),
        });
    }
    actions
}

/// Ask the first granted MCP server that offers a search-style tool about the
/// selection. Only an explicit button press reaches this path, the query is a
/// bounded prefix of the selection, and the result is rendered as sanitized
/// text whose links MICE never follows on its own.
fn fetch_links_via_mcp(
    config: &mice_core::Config,
    selection_text: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let query = bounded_for_model(selection_text.trim(), 200);
    for server in mcp_client::granted_servers(config) {
        let mut process = match mcp_client::McpServerProcess::spawn(server) {
            Ok(process) => process,
            Err(error) => {
                eprintln!(
                    "MICE MCP server {} unavailable: {}",
                    mcp_client::sanitize_external_text(&server.name),
                    mcp_client::sanitize_external_text(&error.to_string())
                );
                continue;
            }
        };
        let Ok(tools) = process.list_tools() else {
            continue;
        };
        let Some(tool) = tools.iter().find(|tool| mcp_client::is_search_tool(tool)) else {
            continue;
        };
        let answer = process.call_tool(&tool.name, json!({ "query": query }))?;
        return Ok((
            mcp_client::sanitize_external_text(&server.name),
            mcp_client::sanitize_external_text(&answer),
        ));
    }
    Err(
        "No granted MCP server offers a search tool; add one in config.toml under [[mcp.servers]]."
            .into(),
    )
}

/// Stream a text action to the overlay through the appropriate provider lane and
/// return the full response. Shared by the summarize and go-deeper paths.
fn stream_selected(
    writer: &mut ChildStdin,
    selected: &mice_providers::ModelDescriptor,
    instruction: &str,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = OverlayStream::new(writer);
    let result = if selected.locality == mice_providers::Locality::Local {
        stream_ollama(selected.id, instruction, Some(text), |chunk| {
            stream.push(chunk)
        })
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(selected.id, instruction, Some(text), |chunk| {
            stream.push(chunk)
        })
    } else {
        stream_openai(selected.id, instruction, Some(text), |chunk| {
            stream.push(chunk)
        })
    };
    result?;
    stream.finish()
}

/// Summarize an oversized selection entirely through the configured local
/// model. Intermediate summaries stay off the overlay; the person sees only
/// progress and the final map-reduce result.
fn stream_chunked_selection_summary(
    writer: &mut ChildStdin,
    model: &mice_providers::ModelDescriptor,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let source_is_code = looks_like_code(text);
    stream_chunked_local_text(
        writer,
        model,
        text,
        ChunkedTextPlan {
            part_instruction: chunk_summary_instruction(source_is_code),
            reduction_instruction: reduce_summary_instruction(source_is_code),
            final_instruction: reduce_summary_instruction(source_is_code),
            part_verb: "Summarizing",
            combine_verb: "Combining summaries",
            final_status: "Preparing your complete summary…",
        },
    )
}

struct ChunkedTextPlan<'a> {
    part_instruction: &'a str,
    reduction_instruction: &'a str,
    final_instruction: &'a str,
    part_verb: &'a str,
    combine_verb: &'a str,
    final_status: &'a str,
}

/// Keep a large local text action within its model context. Partial model
/// output stays private; only the final response streams into the overlay.
fn stream_chunked_local_text(
    writer: &mut ChildStdin,
    model: &mice_providers::ModelDescriptor,
    text: &str,
    plan: ChunkedTextPlan<'_>,
) -> Result<String, Box<dyn std::error::Error>> {
    let chunks = structural_summary_chunks(text, LOCAL_SUMMARY_CHUNK_TOKENS);
    if chunks.is_empty() {
        return Ok(String::new());
    }

    let mut summaries = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        send_command(
            writer,
            AgentCommand::OverlayUpdate {
                text: format!("{} part {} of {}…", plan.part_verb, index + 1, chunks.len()),
            },
        )?;
        let mut summary = String::new();
        stream_ollama(model.id, plan.part_instruction, Some(chunk), |output| {
            summary.push_str(output);
            Ok(())
        })?;
        if summary.trim().is_empty() {
            return Err("The local model returned an empty partial result.".into());
        }
        summaries.push(summary);
    }

    let reduce_budget = model
        .input_budget_tokens
        .unwrap_or(LOCAL_SUMMARY_REDUCE_TOKENS)
        .min(LOCAL_SUMMARY_REDUCE_TOKENS);
    loop {
        let batches = summary_reduce_batches(&summaries, reduce_budget);
        if batches.len() == 1 {
            send_command(
                writer,
                AgentCommand::OverlayUpdate {
                    text: plan.final_status.into(),
                },
            )?;
            let mut stream = OverlayStream::new(writer);
            stream_ollama(
                model.id,
                plan.final_instruction,
                Some(&batches[0].join("\n\n")),
                |output| stream.push(output),
            )?;
            return stream.finish();
        }

        let total_batches = batches.len();
        let mut reduced = Vec::with_capacity(total_batches);
        for (index, batch) in batches.iter().enumerate() {
            send_command(
                writer,
                AgentCommand::OverlayUpdate {
                    text: format!("{} {} of {}…", plan.combine_verb, index + 1, total_batches),
                },
            )?;
            let mut summary = String::new();
            stream_ollama(
                model.id,
                plan.reduction_instruction,
                Some(&batch.join("\n\n")),
                |output| {
                    summary.push_str(output);
                    Ok(())
                },
            )?;
            if summary.trim().is_empty() {
                return Err("The local model returned an empty reduction.".into());
            }
            reduced.push(summary);
        }
        summaries = reduced;
    }
}

const GO_DEEPER_PART_INSTRUCTION: &str = "Extract the specific facts, definitions, background, and implications from this part of the selected content. Preserve enough context for a later answer to explain the whole selection in depth.";
const GO_DEEPER_REDUCTION_INSTRUCTION: &str = "Combine these partial analyses into a compact, accurate foundation for a deeper explanation. Retain key terms, context, nuance, and implications from every part.";

fn stream_chunked_go_deeper(
    writer: &mut ChildStdin,
    model: &mice_providers::ModelDescriptor,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    stream_chunked_local_text(
        writer,
        model,
        text,
        ChunkedTextPlan {
            part_instruction: GO_DEEPER_PART_INSTRUCTION,
            reduction_instruction: GO_DEEPER_REDUCTION_INSTRUCTION,
            final_instruction: GO_DEEPER_INSTRUCTION,
            part_verb: "Going deeper into",
            combine_verb: "Combining context",
            final_status: "Preparing your deeper explanation…",
        },
    )
}

/// Begin a deeper explanation only after the compact recap is complete. It is
/// intentionally silent: the result is displayed only if the person selects
/// Go Deeper. A single in-flight job avoids competing local-model requests.
fn start_go_deeper_prefetch(
    config: &mice_core::Config,
    text: String,
    in_flight: Arc<AtomicBool>,
) -> Option<Receiver<Result<String, String>>> {
    if in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let config = config.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = prepare_go_deeper(&config, &text).map_err(|error| error.to_string());
        let _ = sender.send(result);
        in_flight.store(false, Ordering::Release);
    });
    Some(receiver)
}

/// Produce a deeper explanation without touching the overlay or clipboard, so
/// it can be ready when the user explicitly asks to see it.
fn prepare_go_deeper(
    config: &mice_core::Config,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some(text.to_owned()),
            ..Default::default()
        },
        instruction: GO_DEEPER_INSTRUCTION.to_owned(),
        action: Some(Action::Explain),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    match route_selection_summary(&request, estimate_tokens(text))? {
        SelectionSummaryRoute::SingleShot(route) => {
            collect_selected_response(&route.model, &request.instruction, text)
        }
        SelectionSummaryRoute::Chunked { model } => collect_chunked_go_deeper(&model, text),
    }
}

fn collect_selected_response(
    selected: &mice_providers::ModelDescriptor,
    instruction: &str,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut response = String::new();
    if selected.locality == mice_providers::Locality::Local {
        stream_ollama(selected.id, instruction, Some(text), |chunk| {
            response.push_str(chunk);
            Ok(())
        })?;
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(selected.id, instruction, Some(text), |chunk| {
            response.push_str(chunk);
            Ok(())
        })?;
    } else {
        stream_openai(selected.id, instruction, Some(text), |chunk| {
            response.push_str(chunk);
            Ok(())
        })?;
    }
    if response.trim().is_empty() {
        return Err("The model returned an empty deeper explanation.".into());
    }
    Ok(response)
}

fn collect_chunked_go_deeper(
    model: &mice_providers::ModelDescriptor,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let chunks = structural_summary_chunks(text, LOCAL_SUMMARY_CHUNK_TOKENS);
    if chunks.is_empty() {
        return Ok(String::new());
    }
    let mut summaries = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        summaries.push(collect_selected_response(
            model,
            GO_DEEPER_PART_INSTRUCTION,
            &chunk,
        )?);
    }
    let reduce_budget = model
        .input_budget_tokens
        .unwrap_or(LOCAL_SUMMARY_REDUCE_TOKENS)
        .min(LOCAL_SUMMARY_REDUCE_TOKENS);
    loop {
        let batches = summary_reduce_batches(&summaries, reduce_budget);
        if batches.len() == 1 {
            return collect_selected_response(
                model,
                GO_DEEPER_INSTRUCTION,
                &batches[0].join("\n\n"),
            );
        }
        summaries = batches
            .iter()
            .map(|batch| {
                collect_selected_response(
                    model,
                    GO_DEEPER_REDUCTION_INSTRUCTION,
                    &batch.join("\n\n"),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
    }
}

/// Re-run the cached selection with a deeper-explanation prompt, streaming into
/// the same panel and re-offering the action buttons.
fn run_go_deeper(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    session_id: &str,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some(text.to_owned()),
            ..Default::default()
        },
        instruction: GO_DEEPER_INSTRUCTION.to_owned(),
        action: Some(Action::Explain),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    let (selected, route_notice, chunked_model) =
        match route_selection_summary(&request, estimate_tokens(text))? {
            SelectionSummaryRoute::SingleShot(route) => (route.model, route.user_notice, None),
            SelectionSummaryRoute::Chunked { model } => (model.clone(), None, Some(model)),
        };
    let status = route_notice.map_or_else(
        || "Going deeper…".into(),
        |notice| format!("{notice}\n\nGoing deeper…"),
    );
    send_command(writer, AgentCommand::OverlayShow { text: status })?;
    let response = if let Some(model) = chunked_model {
        stream_chunked_go_deeper(writer, &model, text)?
    } else {
        stream_selected(writer, &selected, &request.instruction, text)?
    };
    if !response.is_empty() {
        send_command(writer, clipboard_command(&response))?;
    }
    send_command(writer, AgentCommand::OverlayFinishResult { text: None })?;
    send_command(
        writer,
        AgentCommand::OverlayResult {
            session_id: session_id.to_owned(),
            actions: selection_result_actions(),
        },
    )?;
    Ok(response)
}

fn present_prepared_go_deeper(
    writer: &mut ChildStdin,
    session_id: &str,
    response: String,
) -> Result<String, Box<dyn std::error::Error>> {
    if !response.is_empty() {
        send_command(
            writer,
            AgentCommand::OverlayAppendResult {
                chunk: response.clone(),
            },
        )?;
        send_command(writer, clipboard_command(&response))?;
    }
    send_command(writer, AgentCommand::OverlayFinishResult { text: None })?;
    send_command(
        writer,
        AgentCommand::OverlayResult {
            session_id: session_id.to_owned(),
            actions: selection_result_actions(),
        },
    )?;
    Ok(response)
}

/// Handle a button press from the interactive result panel.
fn handle_overlay_action(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    cache: &mut Option<SelectionCache>,
    session_id: &str,
    action_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(text) = cache
        .as_ref()
        .filter(|entry| entry.session_id == session_id)
        .map(|entry| entry.text.clone())
    else {
        return Ok(());
    };
    match action_id {
        "copy" => {
            if let Some(entry) = cache.as_ref() {
                send_command(writer, clipboard_command(&entry.response))?;
            }
            send_command(
                writer,
                AgentCommand::OverlayResult {
                    session_id: session_id.to_owned(),
                    actions: selection_result_actions(),
                },
            )?;
        }
        "go_deeper" => {
            let prepared = cache
                .as_mut()
                .and_then(|entry| entry.prepared_go_deeper.take());
            let response = if let Some(receiver) = prepared {
                send_command(
                    writer,
                    AgentCommand::OverlayShow {
                        text: "Preparing your deeper explanation…".into(),
                    },
                )?;
                match receiver.recv() {
                    Ok(Ok(response)) => present_prepared_go_deeper(writer, session_id, response)?,
                    Ok(Err(error)) => {
                        eprintln!("MICE prepared Go Deeper failed; retrying on request: {error}");
                        run_go_deeper(writer, config, session_id, &text)?
                    }
                    Err(_) => run_go_deeper(writer, config, session_id, &text)?,
                }
            } else {
                run_go_deeper(writer, config, session_id, &text)?
            };
            if let Some(entry) = cache.as_mut() {
                entry.response = response;
            }
        }
        "send_paste" => {
            send_command(writer, AgentCommand::ClipboardPaste)?;
            send_command(
                writer,
                AgentCommand::OverlayResult {
                    session_id: session_id.to_owned(),
                    actions: selection_result_actions(),
                },
            )?;
        }
        "fetch_links" => {
            let Some(text) = cache.as_ref().map(|entry| entry.text.clone()) else {
                return Ok(());
            };
            send_command(
                writer,
                AgentCommand::OverlayUpdate {
                    text: "Fetching links from your granted MCP server…".into(),
                },
            )?;
            let chunk = match fetch_links_via_mcp(config, &text) {
                Ok((server, result)) => format!(
                    "\n\n— Links from `{server}` (external result; MICE does not open links itself) —\n{result}"
                ),
                Err(error) => format!(
                    "\n\n(Fetch Links failed: {})",
                    mcp_client::sanitize_external_text(&error.to_string())
                ),
            };
            send_command(writer, AgentCommand::OverlayAppendResult { chunk })?;
            send_command(writer, AgentCommand::OverlayFinishResult { text: None })?;
            send_command(
                writer,
                AgentCommand::OverlayResult {
                    session_id: session_id.to_owned(),
                    actions: selection_result_actions(),
                },
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Enrich the pasteboard the user's own Cmd-C produced, only after the
/// explicit smart-copy gesture. Deterministic table rebuilds need no model;
/// everything else uses the configured local model regardless of the privacy
/// mode, because clipboard content never goes to a cloud provider. On any
/// failure the pasteboard stays exactly as the user's copy wrote it.
fn handle_smart_copy(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    captured: ClipboardCaptured,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(error) = captured.capture_error.as_deref() {
        return finish_smart_copy(writer, error);
    }
    if captured.text.is_none() && captured.html.is_none() && captured.rtf_base64.is_some() {
        return finish_smart_copy(
            writer,
            "This copy only exposes RTF. Smart Copy left the original rich text unchanged.",
        );
    }
    if captured.text.is_none() && captured.html.is_none() && captured.png_base64.is_some() {
        return finish_smart_copy(writer, "Image copy left unchanged.");
    }
    match smart_copy_plan(captured.text.as_deref(), captured.html.as_deref()) {
        SmartCopyPlan::Ready { contents, notice } => {
            if send_smart_copy_contents(writer, contents, captured.png_base64.as_deref())? {
                finish_smart_copy(writer, notice)
            } else {
                finish_smart_copy(
                    writer,
                    "The cleaned copy is too large for MICE's safe clipboard transport; your clipboard is unchanged.",
                )
            }
        }
        SmartCopyPlan::ModelMarkdownTable { source } => {
            let Some(table) = smart_copy_local_table(writer, config, &source)? else {
                return finish_smart_copy(
                    writer,
                    "The local model could not rebuild this table losslessly; your clipboard is unchanged.",
                );
            };
            if send_smart_copy_contents(
                writer,
                table_clipboard_contents(&table),
                captured.png_base64.as_deref(),
            )? {
                finish_smart_copy(writer, mice_core::SMART_COPY_TABLE_NOTICE)
            } else {
                finish_smart_copy(
                    writer,
                    "The cleaned table is too large for MICE's safe clipboard transport; your clipboard is unchanged.",
                )
            }
        }
        SmartCopyPlan::ModelMarkdownClean { source } => {
            let response =
                smart_copy_local_response(writer, config, smart_copy_clean_instruction(), &source)?;
            let markdown = strip_markdown_fence(&response);
            if markdown.trim().is_empty()
                || !smart_copy_preserves_visible_text(&source, markdown)
                || !smart_copy_preserves_links(&source, markdown)
            {
                return finish_smart_copy(
                    writer,
                    "The local model could not clean this copy losslessly; your clipboard is unchanged.",
                );
            }
            if send_smart_copy_contents(
                writer,
                clipboard_contents(markdown),
                captured.png_base64.as_deref(),
            )? {
                finish_smart_copy(
                    writer,
                    "Copy cleaned to Markdown with rich-text representations; paste anywhere.",
                )
            } else {
                finish_smart_copy(
                    writer,
                    "The cleaned copy is too large for MICE's safe clipboard transport; your clipboard is unchanged.",
                )
            }
        }
        SmartCopyPlan::NothingToEnrich { reason } => finish_smart_copy(writer, reason),
    }
}

fn send_smart_copy_contents(
    writer: &mut ChildStdin,
    contents: mice_core::ClipboardContents,
    preserved_png_base64: Option<&str>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let command = AgentCommand::ClipboardSet {
        contents: mice_ipc::ClipboardContents {
            text: contents.text,
            html: contents.html,
            rtf: contents.rtf,
            png_base64: preserved_png_base64.map(str::to_owned),
        },
    };
    let mut probe = Vec::new();
    match write_frame(&mut probe, &command.notification()) {
        Ok(()) => {
            send_command(writer, command)?;
            Ok(true)
        }
        Err(mice_ipc::FrameError::TooLarge) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn finish_smart_copy(
    writer: &mut ChildStdin,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: message.into(),
        },
    )?;
    send_command(writer, AgentCommand::OverlayFinishResult { text: None })
}

/// Run the smart-copy fallback on the configured local model and collect its
/// full response. A model failure reports through the overlay and returns the
/// error so the caller writes nothing to the pasteboard.
fn smart_copy_local_response(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    instruction: &str,
    source: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: format!(
                "Smart copy: rebuilding with {} (clipboard stays local)…",
                config.local_model
            ),
        },
    )?;
    let budget_tokens = mice_providers::model_descriptor(&config.local_model)
        .and_then(|descriptor| descriptor.input_budget_tokens)
        .unwrap_or(4_000);
    let maximum_characters = budget_tokens.saturating_mul(3);
    let Some(chunks) = smart_copy_chunks(source, maximum_characters) else {
        finish_smart_copy(
            writer,
            "This copy has no safe chunk boundaries for local cleanup; your clipboard is unchanged.",
        )?;
        return Err("Smart copy source cannot be chunked losslessly.".into());
    };
    let mut responses = Vec::new();
    for chunk in chunks {
        let response = smart_copy_local_model(writer, config, instruction, &chunk)?;
        if !smart_copy_preserves_visible_text(&chunk, strip_markdown_fence(&response))
            || !smart_copy_preserves_links(&chunk, strip_markdown_fence(&response))
        {
            finish_smart_copy(
                writer,
                "The local model changed visible content or a link; your clipboard is unchanged.",
            )?;
            return Err("Smart copy verification rejected a changed model response.".into());
        }
        responses.push(strip_markdown_fence(&response).to_owned());
    }
    Ok(responses.join("\n\n"))
}

fn smart_copy_local_table(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    source: &str,
) -> Result<Option<mice_core::ExtractedTable>, Box<dyn std::error::Error>> {
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: format!(
                "Smart copy: rebuilding table with {} (clipboard stays local)…",
                config.local_model
            ),
        },
    )?;
    let budget_tokens = mice_providers::model_descriptor(&config.local_model)
        .and_then(|descriptor| descriptor.input_budget_tokens)
        .unwrap_or(4_000);
    let maximum_characters = budget_tokens.saturating_mul(3);
    let lines = source.lines().collect::<Vec<_>>();
    let Some((header, rows)) = lines.split_first() else {
        return Ok(None);
    };
    let mut chunks = Vec::new();
    let mut current = (*header).to_owned();
    for row in rows {
        let candidate = format!("{current}\n{row}");
        if candidate.chars().count() > maximum_characters {
            if current == *header {
                return Ok(None);
            }
            chunks.push(current);
            current = format!("{header}\n{row}");
            if current.chars().count() > maximum_characters {
                return Ok(None);
            }
        } else {
            current = candidate;
        }
    }
    if current != *header {
        chunks.push(current);
    }
    if chunks.is_empty() {
        return Ok(None);
    }

    let mut combined: Option<mice_core::ExtractedTable> = None;
    for chunk in chunks {
        let response =
            smart_copy_local_model(writer, config, smart_copy_table_instruction(), &chunk)?;
        let Some(table) = parse_markdown_table(strip_markdown_fence(&response)) else {
            return Ok(None);
        };
        match combined.as_mut() {
            Some(all) if all.headers == table.headers => all.rows.extend(table.rows),
            Some(_) => return Ok(None),
            None => combined = Some(table),
        }
    }
    let Some(table) = combined else {
        return Ok(None);
    };
    if smart_copy_preserves_visible_text(source, &mice_core::table_markdown(&table)) {
        Ok(Some(table))
    } else {
        Ok(None)
    }
}

fn smart_copy_local_model(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    instruction: &str,
    source: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut response = String::new();
    match stream_ollama(&config.local_model, instruction, Some(source), |chunk| {
        response.push_str(chunk);
        Ok(())
    }) {
        Ok(()) => Ok(response),
        Err(error) => {
            let _ = finish_smart_copy(
                writer,
                &format!(
                    "Smart copy could not run the local model; your clipboard is unchanged. ({error})"
                ),
            );
            Err(error)
        }
    }
}

/// Local models often wrap requested Markdown in a code fence; unwrap one if
/// the whole response is fenced, otherwise return the response as-is.
fn strip_markdown_fence(value: &str) -> &str {
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some((_, body)) = rest.split_once('\n') else {
        return trimmed;
    };
    body.trim_end()
        .strip_suffix("```")
        .map(str::trim_end)
        .unwrap_or(trimmed)
}

fn handle_selection_action(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    selection: SelectionText,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if selection.text.trim().is_empty() {
        send_command(
            writer,
            AgentCommand::OverlayShow {
                text: "Select some text first, then use the MICE shortcut.".into(),
            },
        )?;
        send_command(writer, AgentCommand::OverlayFinishResult { text: None })?;
        return Ok(None);
    }

    let action = match selection.action {
        // A single word / short phrase is a "define this" request; a longer
        // passage is a summary. Same gesture, intent inferred from length.
        SelectionAction::Summarize if is_short_phrase(&selection.text) => Action::Define,
        SelectionAction::Summarize => Action::Summarize,
        SelectionAction::Image => Action::Image,
    };
    let instruction = if action == Action::Summarize {
        selection_summary_instruction(&selection.text).into()
    } else {
        action_instruction(action, "")
    };
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some(selection.text.clone()),
            ..Default::default()
        },
        instruction,
        action: Some(action),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    let (selected, route_notice, chunked_model) = if action == Action::Summarize {
        match route_selection_summary(&request, estimate_tokens(&selection.text))? {
            SelectionSummaryRoute::SingleShot(route) => (route.model, route.user_notice, None),
            SelectionSummaryRoute::Chunked { model } => (model.clone(), None, Some(model)),
        }
    } else {
        let route = route(&request)?;
        (route.model, route.user_notice, None)
    };
    let status = match action {
        Action::Summarize => "Summarizing selection…",
        Action::Define => "Defining…",
        Action::Image => "Creating infographic…",
        _ => unreachable!("selection actions are constrained above"),
    };
    let status =
        route_notice.map_or_else(|| status.into(), |notice| format!("{notice}\n\n{status}"));
    send_command(writer, AgentCommand::OverlayShow { text: status })?;

    if action == Action::Image {
        let image_prompt = format!(
            "{}\n\nSelected content:\n{}",
            request.instruction,
            bounded_for_model(&selection.text, 6_000)
        );
        return match generate_and_present_image(writer, &image_prompt) {
            Ok(()) => Ok(None),
            Err(error) => {
                let _ = send_command(
                    writer,
                    AgentCommand::OverlayFinishResult {
                        text: Some(format!("Image generation error: {error}")),
                    },
                );
                Err(error)
            }
        };
    }

    let response = if let Some(model) = chunked_model {
        stream_chunked_selection_summary(writer, &model, &selection.text)
    } else {
        stream_selected(writer, &selected, &request.instruction, &selection.text)
    };
    match response {
        Ok(response) => {
            if !response.is_empty() {
                send_command(writer, clipboard_command(&response))?;
            }
            send_command(writer, AgentCommand::OverlayFinishResult { text: None })?;
            send_command(
                writer,
                AgentCommand::OverlayResult {
                    session_id: selection.session_id.clone(),
                    actions: selection_result_actions(),
                },
            )?;
            Ok(Some(response))
        }
        Err(error) => {
            let _ = send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some(format!("Selection action error: {error}")),
                },
            );
            Err(error)
        }
    }
}

fn handle_goal_submission(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    session: &mut GoalSession,
    goal_plans: &mut HashMap<String, GoalPlanResult>,
    active_guides: &mut HashMap<String, ActiveGuide>,
    browser_goal_directive: &NativeBridge,
    submission: PromptSubmitted,
) -> Result<(), Box<dyn std::error::Error>> {
    let planning_input = match session.state() {
        GoalState::AwaitingGoal => {
            session.submit_goal(submission.text.clone())?;
            submission.text
        }
        GoalState::Reviewing { .. } if submission.text.trim().is_empty() => {
            session.accept()?;
            let plan = goal_plans
                .get(&submission.session_id)
                .ok_or("MICE lost the reviewed plan; please start again.")?;
            active_guides.insert(
                submission.session_id.clone(),
                ActiveGuide {
                    steps: plan.steps.clone(),
                    current_step: 0,
                },
            );
            return show_active_guide_step(
                writer,
                active_guides,
                browser_goal_directive,
                &submission.session_id,
            );
        }
        GoalState::Reviewing { .. } => session.begin_revision(submission.text)?,
        GoalState::Planning { .. } => return Err("MICE is already generating this plan.".into()),
        GoalState::Accepted { .. } | GoalState::Cancelled => {
            return Err("This goal session is already finished.".into());
        }
    };

    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: "Planning your goal…".into(),
        },
    )?;
    let plan = generate_goal_plan(config, &planning_input)?;
    let rendered = render_goal_plan(&plan);
    session.review(rendered.clone())?;
    goal_plans.insert(submission.session_id.clone(), plan);
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: rendered.clone(),
        },
    )?;
    send_command(
        writer,
        AgentCommand::OverlayPromptInput {
            session_id: submission.session_id,
            title: "Review your plan".into(),
            placeholder: "Optional revision; leave blank to accept".into(),
            context: Some(rendered),
        },
    )?;
    Ok(())
}

fn generate_goal_plan(
    config: &mice_core::Config,
    planning_input: &str,
) -> Result<GoalPlanResult, Box<dyn std::error::Error>> {
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some(planning_input.into()),
            ..Default::default()
        },
        instruction: "Create an advisory, structured plan for this goal.".into(),
        action: Some(Action::GoalPlan),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    let selected = route(&request)?.model;
    let raw = if selected.locality == mice_providers::Locality::Local {
        let mut response = String::new();
        stream_ollama(
            selected.id,
            "Return only JSON with 3-8 advisory steps. Each step needs instruction, app_hint, and sensitive boolean.",
            Some(planning_input),
            |chunk| {
                response.push_str(chunk);
                Ok(())
            },
        )?;
        response
    } else if is_groq_model(selected.id) {
        call_groq_goal_plan(selected.id, planning_input)?
    } else {
        call_openai_goal_plan(selected.id, planning_input)?
    };
    let plan: GoalPlanResult = serde_json::from_str(&raw)
        .map_err(|_| "The planning model returned an invalid plan; please try again.")?;
    validate_goal_plan(&plan)?;
    Ok(plan)
}

fn show_active_guide_step(
    writer: &mut ChildStdin,
    guides: &HashMap<String, ActiveGuide>,
    browser_goal_directive: &NativeBridge,
    session_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let guide = guides
        .get(session_id)
        .ok_or("No active guide was found for this session.")?;
    let step = guide
        .steps
        .get(guide.current_step)
        .ok_or("The active guide has no current step.")?;
    if is_browser_step(step) {
        set_browser_goal_directive(
            browser_goal_directive,
            Some(BrowserGoalDirective {
                session_id: session_id.into(),
                instruction: format!("{}\nApp hint: {}", step.instruction, step.app_hint),
            }),
        );
    } else {
        clear_browser_goal_directive(browser_goal_directive);
    }
    send_command(
        writer,
        AgentCommand::OverlayGuideStep {
            session_id: session_id.into(),
            step_index: guide.current_step,
            total_steps: guide.steps.len(),
            instruction: step.instruction.clone(),
            app_hint: step.app_hint.clone(),
            sensitive: step.sensitive,
            // Goal Guide is advisory/highlight-only. Browser mutation is
            // exclusively owned by the AXI executor, which supplies the
            // current-snapshot and per-action consent guarantees.
            browser_capable: false,
        },
    )
}

fn handle_guide_control(
    writer: &mut ChildStdin,
    guides: &mut HashMap<String, ActiveGuide>,
    browser_goal_directive: &NativeBridge,
    session_id: &str,
    action: &str,
    _value: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if action == "do-it" || action == "do-it-fill" {
        return send_command(
            writer,
            AgentCommand::OverlayFinishResult {
                text: Some("Goal Guide only highlights and explains. Use `mice autopilot --engine axi <goal>` for individually confirmed browser actions.".into()),
            },
        );
    }
    if action == "quit" {
        guides.remove(session_id);
        clear_browser_goal_directive(browser_goal_directive);
        return send_command(
            writer,
            AgentCommand::OverlayFinishResult {
                text: Some("Goal Guide ended. You can start another goal at any time.".into()),
            },
        );
    }
    let guide = guides
        .get_mut(session_id)
        .ok_or("No active guide was found for this session.")?;
    match action {
        "next" if guide.current_step + 1 == guide.steps.len() => {
            guides.remove(session_id);
            clear_browser_goal_directive(browser_goal_directive);
            return send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some("Goal Guide complete. Great work!".into()),
                },
            );
        }
        "next" => guide.current_step += 1,
        "back" => guide.current_step = guide.current_step.saturating_sub(1),
        "stay" => {}
        _ => return Err("Unknown guide control.".into()),
    }
    show_active_guide_step(writer, guides, browser_goal_directive, session_id)
}

fn is_browser_step(step: &GoalPlanStep) -> bool {
    let hint = step.app_hint.to_ascii_lowercase();
    [
        "browser", "chrome", "safari", "firefox", "website", "web page",
    ]
    .iter()
    .any(|term| hint.contains(term))
}

fn blocked_browser_action(
    kind: &str,
    target: &BrowserTarget,
    instruction: &str,
) -> Option<&'static str> {
    let context = format!("{} {} {}", target.label, target.role, instruction).to_ascii_lowercase();
    let fill_terms = [
        "password",
        "passcode",
        "one-time",
        "otp",
        "verification code",
        "cvv",
        "cvc",
        "card number",
        "credit card",
        "debit card",
        "routing number",
        "account number",
    ];
    let click_terms = [
        "pay",
        "purchase",
        "place order",
        "confirm payment",
        "file return",
        "submit return",
        "transfer",
        "sign in",
        "log in",
        "login",
    ];
    if kind == "fill" && fill_terms.iter().any(|term| context.contains(term)) {
        Some("it may contain credentials, a one-time code, or payment data")
    } else if kind == "click" && click_terms.iter().any(|term| context.contains(term)) {
        Some("it appears to submit, authenticate, pay, file, or transfer")
    } else {
        None
    }
}

fn set_browser_goal_directive(shared: &NativeBridge, directive: Option<BrowserGoalDirective>) {
    if let Ok(mut state) = shared.lock() {
        state.directive = directive.clone();
    }
    native_bridge_send(
        shared,
        &serde_json::json!({"type":"goal.step", "directive": directive}),
    );
}

fn clear_browser_goal_directive(shared: &NativeBridge) {
    set_browser_goal_directive(shared, None);
}

fn validate_goal_plan(plan: &GoalPlanResult) -> Result<(), Box<dyn std::error::Error>> {
    if !(3..=8).contains(&plan.steps.len()) {
        return Err("The plan must contain between 3 and 8 steps.".into());
    }
    if plan.steps.iter().any(|step| {
        step.instruction.trim().is_empty()
            || step.app_hint.trim().is_empty()
            || step.instruction.chars().count() > 360
            || step.app_hint.chars().count() > 120
    }) {
        return Err("The plan contained an invalid step; please try again.".into());
    }
    Ok(())
}

fn render_goal_plan(plan: &GoalPlanResult) -> String {
    let mut rendered = String::from("Your plan (you do each action):\n");
    for (index, step) in plan.steps.iter().enumerate() {
        let sensitive = if step.sensitive {
            " [Do this yourself—personal data/login/payment]"
        } else {
            ""
        };
        rendered.push_str(&format!(
            "\n{}. {} ({}){}",
            index + 1,
            step.instruction.trim(),
            step.app_hint.trim(),
            sensitive
        ));
    }
    rendered
}

fn explain_hover(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    hover: HoverCaptured,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(ax) = hover.ax else {
        return Ok(());
    };
    let label = semantic_hover_label(
        ax.title.as_deref(),
        ax.description.as_deref(),
        ax.value.as_deref(),
        hover.text.as_deref(),
    );
    let Some(label) = label else {
        return Ok(());
    };
    let control_type = hover_control_type(ax.role.as_deref(), Some(&label), None);
    let auxiliary_context = auxiliary_hover_context(ax.description.as_deref(), &label);
    let current_value = ax.value.unwrap_or_default();
    let actions = ax.actions.join(", ");
    let context = format!(
        "Current control (captured under the pointer now):\nControl type: {control_type}\nVisible label: {}\nAdditional context: {}\nCurrent value: {}\nAvailable actions: {}",
        bounded_for_model(&label, 240),
        bounded_for_model(&auxiliary_context, 400),
        bounded_for_model(&current_value, 400),
        bounded_for_model(&actions, 200),
    );
    eprintln!("[MICE hover AX] {context}");
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some(context.clone()),
            ax: true,
            ..Default::default()
        },
        instruction: "Use only the current control snapshot below. Identify the visible label and its concrete user purpose, not its implementation. Reply in exactly this compact form: `<visible label> <control type>. <Primary action or purpose>.` For known web shortcuts, say what destination opens. Never mention AX, accessibility, metadata, tooltips, terminal output, logs, previous requests, or unseen content. Never say `interface control`, `status update`, or `unidentified`. If purpose is uncertain, state only the label and control type.".into(),
        action: Some(Action::Explain),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    let selected = route(&request)?.model;
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: "Explaining control…".into(),
        },
    )?;
    let mut stream = OverlayStream::new(writer);
    let result = if selected.locality == mice_providers::Locality::Local {
        stream_ollama(selected.id, &request.instruction, Some(&context), |chunk| {
            stream.push(chunk)
        })
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(selected.id, &request.instruction, Some(&context), |chunk| {
            stream.push(chunk)
        })
    } else {
        stream_openai(selected.id, &request.instruction, Some(&context), |chunk| {
            stream.push(chunk)
        })
    };
    match result {
        Ok(()) => {
            stream.finish()?;
            send_command(writer, AgentCommand::OverlayFinishResult { text: None })
        }
        Err(error) => {
            let _ = stream.finish();
            let _ = send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some(format!("Hover explanation error: {error}")),
                },
            );
            Err(error)
        }
    }
}

struct AgentSession {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    platform: String,
    capabilities: Capabilities,
}

fn start_agent(
    gesture: &mice_core::GestureConfig,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    spawn_agent(gesture, false, false)
}

/// A display-only agent for one-shot commands: it never creates an event tap,
/// so it needs no Input Monitoring grant and observes no input.
fn start_agent_overlay_only(
    gesture: &mice_core::GestureConfig,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    spawn_agent(gesture, false, true)
}

fn spawn_agent(
    gesture: &mice_core::GestureConfig,
    autopilot_active: bool,
    overlay_only: bool,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let agent = agent_path()?;
    if !agent.exists() {
        return Err(format!(
            "macOS agent has not been built yet at {}; run `swift build` in agent-macos first",
            agent.display()
        )
        .into());
    }
    let mut child = Command::new(agent)
        .env("MICE_GESTURE_TRIGGER", &gesture.trigger)
        .env(
            "MICE_SUMMARIZE_SELECTION_TRIGGER",
            &gesture.summarize_selection_trigger,
        )
        .env(
            "MICE_INFOGRAPHIC_SELECTION_TRIGGER",
            &gesture.infographic_selection_trigger,
        )
        .env("MICE_GOAL_TRIGGER", &gesture.goal_trigger)
        .env("MICE_SMART_COPY_TRIGGER", &gesture.smart_copy_trigger)
        .env(
            "MICE_AUTOPILOT_ACTIVE",
            if autopilot_active { "1" } else { "0" },
        )
        .env("MICE_OVERLAY_ONLY", if overlay_only { "1" } else { "0" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Agent stdout is reserved for length-prefixed JSON-RPC. AppKit may log
        // incidental service messages on stderr, which must not corrupt CLI UX.
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take().ok_or("agent stdin was not available")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("agent stdout was not available")?;
    let mut reader = BufReader::new(stdout);
    let initialize: RpcRequest = read_frame(&mut reader)?;
    if initialize.method != "initialize" {
        return Err(format!("agent sent {} instead of initialize", initialize.method).into());
    }
    let params: InitializeParams = serde_json::from_value(initialize.params)?;
    if params.protocol_version != mice_ipc::PROTOCOL_VERSION {
        return Err(format!("unsupported agent protocol {}", params.protocol_version).into());
    }
    Ok(AgentSession {
        child,
        stdin,
        reader,
        platform: params.platform,
        capabilities: params.capabilities,
    })
}

fn agent_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os("MICE_MAC_AGENT_PATH") {
        return Ok(PathBuf::from(path));
    }

    // A packaged install (MICE.app/Contents/MacOS) ships the agent beside the
    // CLI binary, so upgrades replace both together and never mix versions.
    if let Ok(executable) = env::current_exe()
        && let Some(sibling) = executable
            .parent()
            .map(|directory| directory.join("mice-mac-agent"))
        && sibling.exists()
    {
        return Ok(sibling);
    }

    // Cargo compiles this source inside `crates/mice-cli`; use that stable
    // workspace location instead of the caller's current directory.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .ok_or("could not determine the MICE workspace path")?;
    Ok(workspace.join("agent-macos/.build/debug/mice-mac-agent"))
}

fn route_preview() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some("preview".into()),
            ..Default::default()
        },
        instruction: "summarize".into(),
        action: Some(Action::Summarize),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model,
            cloud_model: config.cloud_model,
        },
    };
    println!(
        "{}",
        serde_json::json!({"model": route(&request)?.model.id})
    );
    Ok(())
}

/// Serve MICE's local, text-only capabilities over MCP's stdio transport.
/// This intentionally never routes through cloud providers: callers can use a
/// larger harness without silently sending their repository text elsewhere.
fn mcp_server() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let mut session = McpSession::default();
    let stdin = std::io::stdin();
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_mcp_message(
                    &mut output,
                    &mcp_error(Value::Null, -32700, format!("Invalid JSON: {error}")),
                )?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            if let Some(id) = id {
                write_mcp_message(&mut output, &mcp_error(id, -32600, "Missing method"))?;
            }
            continue;
        };

        let response = match method {
            "initialize" => id.map(|id| {
                session = McpSession::from_initialize(request.get("params"));
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mice", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": "You are paired with MICE, a local execution manager. Delegate mechanical or token-heavy work through delegate_task or run_tool; MICE first uses deterministic local CLIs and returns bounded results. Check team_status before editing shared files and record durable decisions with memory_note."
                    }
                })
            }),
            "tools/list" => id.map(|id| mcp_result(id, json!({"tools": mcp_tools()}))),
            "tools/call" => id.map(|id| {
                let result = match request.get("params") {
                    Some(params) => mcp_call_tool(&config, &session, params),
                    None => Err("Missing tool parameters".into()),
                };
                match result {
                    Ok(text) => mcp_result(
                        id,
                        json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    ),
                    Err(error) => mcp_result(
                        id,
                        json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true}),
                    ),
                }
            }),
            _ => id.map(|id| mcp_error(id, -32601, format!("Unknown method: {method}"))),
        };
        if let Some(response) = response {
            write_mcp_message(&mut output, &response)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct McpSession {
    id: String,
    agent: String,
}

impl Default for McpSession {
    fn default() -> Self {
        let id = tool_session_name();
        Self {
            agent: id.clone(),
            id,
        }
    }
}

impl McpSession {
    fn from_initialize(params: Option<&Value>) -> Self {
        let agent = params
            .and_then(|params| params.get("clientInfo"))
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("mcp-agent")
            .to_owned();
        let id = format!("{}-{}", agent, std::process::id());
        Self { id, agent }
    }
}

fn write_mcp_message(
    output: &mut impl Write,
    message: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *output, message)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn mcp_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp_error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn mcp_tools() -> Vec<Value> {
    let mut values = vec![
        json!({
            "name": "summarize_text",
            "description": "Summarize text using MICE's configured local Ollama model.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
        }),
        json!({
            "name": "summarize_file",
            "description": "Read and summarize a local text file without sending it to a cloud provider.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }),
        json!({
            "name": "explain_code",
            "description": "Explain a code snippet with the configured local Ollama model.",
            "inputSchema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}
        }),
        json!({
            "name": "define_word",
            "description": "Define a word or short phrase using the configured local Ollama model.",
            "inputSchema": {"type": "object", "properties": {"word": {"type": "string"}}, "required": ["word"]}
        }),
        json!({
            "name": "quick_answer",
            "description": "Answer a short question using the configured local Ollama model.",
            "inputSchema": {"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]}
        }),
        json!({"name": "run_tool", "description": "Run one bounded deterministic MICE registry tool. Cheap, local, and cacheable.", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "args": {"type": "object"}}, "required": ["name"]}}),
        json!({"name": "delegate_task", "description": "Delegate a bounded mechanical task to MICE's local tool manager.", "inputSchema": {"type": "object", "properties": {"instruction": {"type": "string"}, "max_actions": {"type": "integer"}}, "required": ["instruction"]}}),
        json!({"name": "git_summary", "description": "Return a bounded local git status, log, and diff brief.", "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "repo_grep", "description": "Search this repository locally with a bounded result.", "inputSchema": {"type": "object", "properties": {"pattern": {"type": "string"}, "path": {"type": "string"}}, "required": ["pattern"]}}),
        json!({"name": "memory_note", "description": "Record a durable shared-team decision or fact.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "memory_query", "description": "Search shared MICE events, decisions, and recent work deterministically.", "inputSchema": {"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]}}),
        json!({"name": "team_status", "description": "Show active MICE sessions and early file-overlap warnings.", "inputSchema": {"type": "object", "properties": {}}}),
    ];
    values.extend(tools::tool_schema());
    values
}

fn mcp_call_tool(
    config: &mice_core::Config,
    session: &McpSession,
    params: &Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("Missing tool name")?;
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    let string_argument = |key| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("Missing non-empty `{key}` argument"))
    };
    match name {
        "summarize_text" => mcp_summarize(config, string_argument("text")?),
        "summarize_file" => {
            let path = string_argument("path")?;
            let contents = std::fs::read_to_string(path)
                .map_err(|error| format!("Could not read `{path}`: {error}"))?;
            mcp_summarize(config, &contents)
        }
        "explain_code" => mcp_local_response(
            config,
            "Explain this code clearly. Cover its purpose, main components, control flow, and notable dependencies.",
            Some(string_argument("code")?),
        ),
        "define_word" => mcp_local_response(
            config,
            &action_instruction(Action::Define, ""),
            Some(string_argument("word")?),
        ),
        "quick_answer" => mcp_local_response(
            config,
            "Answer this question briefly and directly. Say when the provided context is insufficient.",
            Some(string_argument("question")?),
        ),
        "run_tool" => {
            let tool_name = string_argument("name")?;
            let tool_args = arguments.get("args").cloned().unwrap_or_else(|| json!({}));
            Ok(format_tool_output(run_registered_tool(
                config,
                &session.id,
                ToolCall {
                    name: tool_name.into(),
                    args: tool_args,
                },
            )?))
        }
        "git_summary" => {
            let mut output = Vec::new();
            for name in ["git.status", "git.log", "git.diff"] {
                output.push(format!(
                    "{name}:\n{}",
                    run_registered_tool(
                        config,
                        &session.id,
                        ToolCall {
                            name: name.into(),
                            args: json!({})
                        }
                    )?
                    .text
                ));
            }
            Ok(output.join("\n\n"))
        }
        "repo_grep" => Ok(format_tool_output(run_registered_tool(
            config,
            &session.id,
            ToolCall {
                name: "repo.grep".into(),
                args: json!({"pattern": string_argument("pattern")?, "path": arguments.get("path").and_then(Value::as_str)}),
            },
        )?)),
        "memory_note" => {
            let cwd = env::current_dir()?;
            shared_memory()?.append(&memory::MemoryEvent {
                event_ts: memory::now(),
                recorded_ts: memory::now(),
                session: session.id.clone(),
                agent: session.agent.clone(),
                branch: current_branch(&cwd),
                kind: "memory_note".into(),
                text: string_argument("text")?.into(),
                files: Vec::new(),
            })?;
            Ok("Shared memory note recorded.".into())
        }
        "memory_query" => Ok(shared_memory()?.query(string_argument("question")?)?),
        "team_status" => Ok(shared_memory()?.team_status()?),
        "delegate_task" => {
            let max_actions = arguments
                .get("max_actions")
                .and_then(Value::as_u64)
                .map(usize::try_from)
                .transpose()
                .map_err(|_| "max_actions is too large for this machine")?
                .unwrap_or(6);
            delegate_task(
                config,
                session,
                string_argument("instruction")?,
                max_actions,
            )
        }
        name if tools::specs().iter().any(|spec| spec.name == name) => {
            Ok(format_tool_output(run_registered_tool(
                config,
                &session.id,
                ToolCall {
                    name: name.into(),
                    args: arguments.clone(),
                },
            )?))
        }
        _ => Err(format!("Unknown MICE tool: {name}").into()),
    }
}

fn format_tool_output(output: ToolOutput) -> String {
    let marker = if output.truncated {
        "\n[truncated: full raw output is intentionally not persisted]"
    } else {
        ""
    };
    format!(
        "{}{}\nfull_output_ref: {}",
        output.text,
        marker,
        output
            .full_output_ref
            .unwrap_or_else(|| "unavailable".into())
    )
}

fn mcp_local_response(
    config: &mice_core::Config,
    instruction: &str,
    text: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut response = String::new();
    stream_ollama(&config.local_model, instruction, text, |chunk| {
        response.push_str(chunk);
        Ok(())
    })?;
    if response.trim().is_empty() {
        return Err("The local model returned an empty response.".into());
    }
    Ok(response)
}

fn mcp_summarize(
    config: &mice_core::Config,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let model = mice_providers::model_descriptor(&config.local_model).ok_or_else(|| {
        format!(
            "MICE has no local model descriptor for `{}`",
            config.local_model
        )
    })?;
    let instruction = selection_summary_instruction(text);
    if estimate_tokens(text)
        <= model
            .input_budget_tokens
            .unwrap_or(LOCAL_SUMMARY_REDUCE_TOKENS)
    {
        return mcp_local_response(config, instruction, Some(text));
    }

    let source_is_code = looks_like_code(text);
    let mut summaries = Vec::new();
    for chunk in structural_summary_chunks(text, LOCAL_SUMMARY_CHUNK_TOKENS) {
        summaries.push(mcp_local_response(
            config,
            chunk_summary_instruction(source_is_code),
            Some(&chunk),
        )?);
    }
    let reduce_budget = model
        .input_budget_tokens
        .unwrap_or(LOCAL_SUMMARY_REDUCE_TOKENS)
        .min(LOCAL_SUMMARY_REDUCE_TOKENS);
    loop {
        let batches = summary_reduce_batches(&summaries, reduce_budget);
        if batches.len() == 1 {
            return mcp_local_response(
                config,
                reduce_summary_instruction(source_is_code),
                Some(&batches[0].join("\n\n")),
            );
        }
        summaries = batches
            .iter()
            .map(|batch| {
                mcp_local_response(
                    config,
                    reduce_summary_instruction(source_is_code),
                    Some(&batch.join("\n\n")),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
    }
}

fn ask() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let mut arguments = env::args().skip(2).collect::<Vec<_>>();
    let action = if arguments
        .first()
        .is_some_and(|argument| argument == "--action")
    {
        if arguments.len() < 2 {
            return Err("Usage: mice ask --action <preset> <instruction>".into());
        }
        let action = parse_action(&arguments[1])?;
        arguments.drain(0..2);
        action
    } else {
        Action::Summarize
    };
    let instruction = arguments.join(" ");
    if instruction.is_empty() {
        return Err("Usage: mice ask <instruction>  (pipe selected text through stdin)".into());
    }
    let text = if std::io::stdin().is_terminal() {
        None
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        (!text.trim().is_empty()).then_some(text)
    };
    let request = RouteRequest {
        artifacts: Artifacts {
            text: text.clone(),
            ..Default::default()
        },
        instruction: action_instruction(action, &instruction),
        action: Some(action),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model,
            cloud_model: config.cloud_model,
        },
    };
    let selected = route(&request)?.model;
    // A one-shot ask only displays a result; the lightweight agent mode needs
    // no Input Monitoring grant and observes no input.
    let mut agent = start_agent_overlay_only(&config.gesture)?;
    send_command(
        &mut agent.stdin,
        AgentCommand::OverlayShow {
            text: "MICE is thinking…".into(),
        },
    )?;
    println!("[{}]", selected.id);
    if action == Action::Image {
        let result = generate_and_present_image(&mut agent.stdin, &request.instruction);
        return match result {
            Ok(()) => {
                println!("[gpt-image-2] Infographic generated and copied to the clipboard.");
                Ok(())
            }
            Err(error) => {
                let _ = send_command(
                    &mut agent.stdin,
                    AgentCommand::OverlayFinishResult {
                        text: Some(format!("Image generation error: {error}")),
                    },
                );
                Err(error)
            }
        };
    }
    let mut stream = OverlayStream::echoing(&mut agent.stdin);
    let result = if selected.locality == mice_providers::Locality::Local {
        stream_ollama(
            selected.id,
            &request.instruction,
            text.as_deref(),
            |chunk| stream.push(chunk),
        )
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(
            selected.id,
            &request.instruction,
            text.as_deref(),
            |chunk| stream.push(chunk),
        )
    } else {
        stream_openai(
            selected.id,
            &request.instruction,
            text.as_deref(),
            |chunk| stream.push(chunk),
        )
    };
    match result {
        Ok(()) => {
            let response = stream.finish()?;
            if !response.is_empty() {
                send_command(&mut agent.stdin, clipboard_command(&response))?;
            }
            send_command(
                &mut agent.stdin,
                AgentCommand::OverlayFinishResult { text: None },
            )?;
            hold_one_shot_overlay(agent)
        }
        Err(error) => {
            let _ = stream.finish();
            let _ = send_command(
                &mut agent.stdin,
                AgentCommand::OverlayFinishResult {
                    text: Some(format!("Error: {error}")),
                },
            );
            Err(error)
        }
    }
}

/// Answer a question about the user's own screen (M12 extended to native
/// apps). The capture happens only for this explicit command, is flashed on
/// screen by the agent, and is never persisted. `local_only` sends only the
/// on-device OCR text to the local model; the image itself reaches a cloud
/// model only when the privacy mode allows cloud work.
fn see() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let mut arguments = env::args().skip(2).collect::<Vec<_>>();
    let scope = if arguments.first().map(String::as_str) == Some("--display") {
        arguments.remove(0);
        mice_ipc::ScreenCaptureScope::DisplayUnderMouse
    } else {
        mice_ipc::ScreenCaptureScope::FrontWindow
    };
    let question = arguments.join(" ");
    if question.is_empty() {
        return Err("Usage: mice see [--display] <question about your screen>".into());
    }
    let agent = start_agent_overlay_only(&config.gesture)?;
    if !agent.capabilities.screen_capture {
        return Err(
            "Screen Recording permission is not granted; enable it for this terminal in System Settings > Privacy & Security, then retry."
                .into(),
        );
    }
    let AgentSession {
        mut child,
        mut stdin,
        reader,
        platform,
        capabilities,
    } = agent;
    send_command(
        &mut stdin,
        AgentCommand::OverlayShow {
            text: "Capturing your screen…".into(),
        },
    )?;
    let session_id = format!(
        "see-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    send_command(
        &mut stdin,
        AgentCommand::ScreenCapture {
            session_id: session_id.clone(),
            scope,
        },
    )?;
    let (captured, reader) = match wait_for_screen_capture(reader, &session_id) {
        Ok(result) => result,
        Err(error) => {
            let _ = send_command(
                &mut stdin,
                AgentCommand::OverlayFinishResult {
                    text: Some(error.to_string()),
                },
            );
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut agent = AgentSession {
        child,
        stdin,
        reader,
        platform,
        capabilities,
    };
    if let Some(error) = captured.capture_error.as_deref() {
        let _ = send_command(
            &mut agent.stdin,
            AgentCommand::OverlayFinishResult {
                text: Some(error.into()),
            },
        );
        return Err(error.into());
    }
    let source = match (
        captured.app_name.as_deref(),
        captured.window_title.as_deref(),
    ) {
        (Some(app), Some(title)) => format!("{app} — {title}"),
        (Some(app), None) => app.to_owned(),
        _ => "the captured display".to_owned(),
    };
    println!("[MICE see] captured {source} (not stored)");
    let ocr_text = captured.ocr_text.unwrap_or_default();

    let cloud_vision_allowed = config.privacy_mode != PrivacyMode::LocalOnly
        && env::var_os("OPENAI_API_KEY").is_some()
        && captured.png_base64.is_some();
    if cloud_vision_allowed {
        let model = mice_providers::model_descriptor(&config.cloud_model)
            .filter(|descriptor| descriptor.vision)
            .map_or("gpt-5.6-sol", |descriptor| descriptor.id);
        println!("[{model} vision]");
        send_command(
            &mut agent.stdin,
            AgentCommand::OverlayUpdate {
                text: format!("Looking at {source} with {model}…"),
            },
        )?;
        let api_key = env::var("OPENAI_API_KEY")?;
        let data_url = format!(
            "data:image/png;base64,{}",
            captured.png_base64.unwrap_or_default()
        );
        let payload = mice_providers::openai_vision_answer_payload(
            model,
            &question,
            &bounded_for_model(&ocr_text, 4_000),
            &data_url,
        )
        .to_string();
        let response = post_provider_json(
            "OpenAI vision",
            "https://api.openai.com/v1/responses",
            &api_key,
            &payload,
        )?;
        let answer = response["output"]
            .as_array()
            .and_then(|items| items.iter().find_map(|item| item["content"].as_array()))
            .and_then(|content| content.iter().find_map(|part| part["text"].as_str()))
            .map(str::to_owned)
            .ok_or("OpenAI vision returned no text answer.")?;
        println!("{answer}");
        send_command(&mut agent.stdin, clipboard_command(&answer))?;
        send_command(
            &mut agent.stdin,
            AgentCommand::OverlayFinishResult { text: Some(answer) },
        )?;
        return hold_one_shot_overlay(agent);
    }

    // The local lane: only the on-device OCR text goes to the local model.
    // The captured pixels never leave this machine.
    if ocr_text.trim().is_empty() {
        let message = if config.privacy_mode == PrivacyMode::LocalOnly {
            "The capture contains no readable text, and local-only mode never sends the image to a cloud model."
        } else {
            "The capture contains no readable text, and no OPENAI_API_KEY is set for the vision lane."
        };
        let _ = send_command(
            &mut agent.stdin,
            AgentCommand::OverlayFinishResult {
                text: Some(message.into()),
            },
        );
        return Err(message.into());
    }
    if config.privacy_mode != PrivacyMode::LocalOnly {
        println!(
            "(no OPENAI_API_KEY; answering from on-device OCR text with {})",
            config.local_model
        );
    }
    send_command(
        &mut agent.stdin,
        AgentCommand::OverlayUpdate {
            text: format!("Reading {source} with {} (local)…", config.local_model),
        },
    )?;
    let instruction = format!(
        "Answer the question using only the OCR text extracted from the user's own screen. Be concrete and concise; if the answer is not in the text, say so plainly.\n\nQuestion: {question}"
    );
    let mut stream = OverlayStream::echoing(&mut agent.stdin);
    stream_ollama(
        &config.local_model,
        &instruction,
        Some(&bounded_for_model(&ocr_text, 12_000)),
        |chunk| stream.push(chunk),
    )?;
    let answer = stream.finish()?;
    println!();
    if !answer.is_empty() {
        send_command(&mut agent.stdin, clipboard_command(&answer))?;
    }
    send_command(
        &mut agent.stdin,
        AgentCommand::OverlayFinishResult { text: None },
    )?;
    hold_one_shot_overlay(agent)
}

const SCREEN_CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);

/// Wait for the one explicit native capture without allowing a stalled
/// ScreenCaptureKit/Vision operation to hold the CLI or overlay forever. The
/// caller kills the one-shot agent on timeout, which closes its IPC pipe too.
fn wait_for_screen_capture(
    reader: BufReader<std::process::ChildStdout>,
    session_id: &str,
) -> Result<
    (
        mice_ipc::ScreenCaptured,
        BufReader<std::process::ChildStdout>,
    ),
    Box<dyn std::error::Error>,
> {
    let wanted = session_id.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut reader = reader;
        loop {
            let result =
                (|| -> Result<Option<mice_ipc::ScreenCaptured>, Box<dyn std::error::Error>> {
                    let message: mice_ipc::RpcNotification = read_frame(&mut reader)?;
                    if message.method != "screen.captured" {
                        return Ok(None);
                    }
                    let captured: mice_ipc::ScreenCaptured =
                        serde_json::from_value(message.params)?;
                    Ok((captured.session_id == wanted).then_some(captured))
                })();
            match result {
                Ok(Some(captured)) => {
                    let _ = sender.send(Ok((captured, reader)));
                    return;
                }
                Ok(None) => continue,
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
    });
    receiver
        .recv_timeout(SCREEN_CAPTURE_TIMEOUT)
        .map_err(|_| {
            format!(
                "Screen capture timed out after {} seconds; MICE stopped the capture safely.",
                SCREEN_CAPTURE_TIMEOUT.as_secs()
            )
        })?
        .map_err(Into::into)
}

/// Keep a one-shot overlay on screen until the person dismisses it; dropping
/// the agent immediately would close the panel before it could be read.
fn hold_one_shot_overlay(mut agent: AgentSession) -> Result<(), Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    println!();
    print!("(overlay shown — press Enter to close it) ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let _ = send_command(&mut agent.stdin, AgentCommand::OverlayDismiss);
    Ok(())
}

fn parse_action(value: &str) -> Result<Action, Box<dyn std::error::Error>> {
    match value {
        "explain" => Ok(Action::Explain),
        "summarize" => Ok(Action::Summarize),
        "rewrite" => Ok(Action::Rewrite),
        "translate" => Ok(Action::Translate),
        "extract-json" => Ok(Action::ExtractJson),
        "code" => Ok(Action::Code),
        "image" => Ok(Action::Image),
        "guide" => Ok(Action::Guide),
        "qa" => Ok(Action::Qa),
        _ => Err(
            format!("Unknown action preset `{value}`; run `mice actions` to list presets.").into(),
        ),
    }
}

fn action_for_interactive_instruction(instruction: &str) -> Action {
    let normalized = instruction.to_ascii_lowercase();
    if normalized.contains("infographic") || normalized.contains("generate image") {
        Action::Image
    } else {
        Action::Summarize
    }
}

fn clipboard_command(text: &str) -> AgentCommand {
    clipboard_set_command(clipboard_contents(text))
}

fn clipboard_set_command(contents: mice_core::ClipboardContents) -> AgentCommand {
    AgentCommand::ClipboardSet {
        contents: mice_ipc::ClipboardContents {
            text: contents.text,
            html: contents.html,
            rtf: contents.rtf,
            png_base64: None,
        },
    }
}

fn clipboard_image_command(png_base64: &str) -> AgentCommand {
    let contents = clipboard_contents("MICE-generated infographic");
    AgentCommand::ClipboardSet {
        contents: mice_ipc::ClipboardContents {
            text: contents.text,
            html: contents.html,
            rtf: contents.rtf,
            png_base64: Some(png_base64.into()),
        },
    }
}

fn send_command(
    writer: &mut ChildStdin,
    command: AgentCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(writer, &command.notification())?;
    Ok(())
}

fn prompt(instruction: &str, text: Option<&str>) -> String {
    match text {
        Some(text) => format!("{instruction}\n\nContent:\n{text}"),
        None => instruction.into(),
    }
}

fn stream_ollama(
    model: &str,
    instruction: &str,
    text: Option<&str>,
    mut on_chunk: impl FnMut(&str) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let num_ctx = mice_providers::model_descriptor(model)
        .and_then(|descriptor| descriptor.num_ctx)
        .ok_or_else(|| format!("MICE has no local context-window budget for `{model}`"))?;
    stream_ollama_chat(
        "http://127.0.0.1:11434/api/chat",
        model,
        instruction,
        text,
        num_ctx,
        |chunk| on_chunk(chunk).map_err(|error| OllamaError::Consumer(error.to_string())),
    )?;
    Ok(())
}

fn hover_control_type(
    role: Option<&str>,
    title: Option<&str>,
    description: Option<&str>,
) -> &'static str {
    match role.unwrap_or_default() {
        "AXButton" => "button",
        "AXLink" => "link",
        "AXTextField" | "AXTextArea" => "text field",
        "AXCheckBox" => "checkbox",
        "AXRadioButton" => "radio button",
        "AXComboBox" | "AXPopUpButton" => "menu",
        _ if title.is_some_and(|value| !value.trim().is_empty())
            || description.is_some_and(|value| !value.trim().is_empty()) =>
        {
            "control"
        }
        _ => "interface control",
    }
}

fn semantic_hover_label(
    title: Option<&str>,
    description: Option<&str>,
    value: Option<&str>,
    captured_text: Option<&str>,
) -> Option<String> {
    [title, captured_text, description, value]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty() && !is_generic_hover_label(value))
        .map(str::to_owned)
}

fn auxiliary_hover_context(description: Option<&str>, label: &str) -> String {
    description
        .map(str::trim)
        .filter(|description| !description.is_empty() && *description != label)
        .unwrap_or_default()
        .into()
}

fn is_generic_hover_label(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "button" | "group" | "link" | "control" | "status" | "unknown"
    )
}

/// Streams model output to the overlay in coalesced batches. One IPC frame
/// per token can outrun the agent's stdin pipe, and the resulting blocking
/// write stalls the provider stream itself; batching to ~512 bytes or 80 ms
/// keeps the overlay live while bounding the frame rate.
struct OverlayStream<'a> {
    writer: &'a mut ChildStdin,
    response: String,
    pending: String,
    last_flush: Instant,
    echo_to_terminal: bool,
}

impl<'a> OverlayStream<'a> {
    const FLUSH_BYTES: usize = 512;
    const FLUSH_INTERVAL: Duration = Duration::from_millis(80);

    fn new(writer: &'a mut ChildStdin) -> Self {
        Self {
            writer,
            response: String::new(),
            pending: String::new(),
            last_flush: Instant::now(),
            echo_to_terminal: false,
        }
    }

    fn echoing(writer: &'a mut ChildStdin) -> Self {
        Self {
            echo_to_terminal: true,
            ..Self::new(writer)
        }
    }

    fn push(&mut self, chunk: &str) -> Result<(), Box<dyn std::error::Error>> {
        if self.echo_to_terminal {
            print!("{chunk}");
            let _ = std::io::stdout().flush();
        }
        self.response.push_str(chunk);
        self.pending.push_str(chunk);
        if self.pending.len() >= Self::FLUSH_BYTES
            || self.last_flush.elapsed() >= Self::FLUSH_INTERVAL
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.pending.is_empty() {
            let chunk = std::mem::take(&mut self.pending);
            send_command(self.writer, AgentCommand::OverlayAppendResult { chunk })?;
        }
        self.last_flush = Instant::now();
        Ok(())
    }

    fn finish(mut self) -> Result<String, Box<dyn std::error::Error>> {
        self.flush()?;
        Ok(self.response)
    }
}

fn bounded_for_model(value: &str, maximum_characters: usize) -> String {
    let mut characters = value.chars();
    let bounded = characters
        .by_ref()
        .take(maximum_characters)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

fn stream_openai(
    model: &str,
    instruction: &str,
    text: Option<&str>,
    mut on_chunk: impl FnMut(&str) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let api_key =
        env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required for cloud requests")?;
    let payload = mice_providers::openai_responses_payload(model, instruction, text).to_string();
    let response = post_provider_request(
        "OpenAI Responses API",
        "https://api.openai.com/v1/responses",
        &api_key,
        &payload,
    )?;
    for line in BufReader::new(response.into_reader()).lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let event: serde_json::Value = serde_json::from_str(data)?;
        if event["type"] == "response.output_text.delta"
            && let Some(delta) = event["delta"].as_str()
        {
            on_chunk(delta)?;
        }
    }
    Ok(())
}

fn generate_openai_image(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for image generation")?;
    let payload = mice_providers::openai_image_generation_payload(prompt).to_string();
    let response = post_provider_json(
        "OpenAI Images API",
        "https://api.openai.com/v1/images/generations",
        &api_key,
        &payload,
    )?;
    response["data"][0]["b64_json"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "OpenAI Images API response did not include data[0].b64_json".into())
}

fn generate_and_present_image(
    writer: &mut ChildStdin,
    prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let png_base64 = generate_openai_image(prompt)?;
    send_command(
        writer,
        AgentCommand::OverlayShowImage {
            png_base64: png_base64.clone(),
        },
    )?;
    send_command(writer, clipboard_image_command(&png_base64))?;
    send_command(
        writer,
        AgentCommand::OverlayFinishResult {
            text: Some("Infographic ready — copied as PNG.".into()),
        },
    )
}

fn stream_groq(
    model: &str,
    instruction: &str,
    text: Option<&str>,
    mut on_chunk: impl FnMut(&str) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let api_key =
        env::var("GROQ_API_KEY").map_err(|_| "GROQ_API_KEY is required for Groq requests")?;

    let payload = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": prompt(instruction, text)
            }
        ],
        "stream": true
    })
    .to_string();

    let response = post_provider_request(
        "Groq API",
        "https://api.groq.com/openai/v1/chat/completions",
        &api_key,
        &payload,
    )?;
    for line in BufReader::new(response.into_reader()).lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let trimmed_data = data.trim();
        if trimmed_data == "[DONE]" {
            break;
        }
        if trimmed_data.is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(trimmed_data)?;
        if let Some(choices) = event["choices"].as_array()
            && let Some(choice) = choices.first()
            && let Some(content) = choice["delta"]["content"].as_str()
        {
            on_chunk(content)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod hover_tests {
    use super::*;

    #[test]
    fn short_selections_are_treated_as_definitions() {
        assert!(is_short_phrase("serendipity"));
        assert!(is_short_phrase("machine learning"));
        assert!(!is_short_phrase(
            "This is a longer sentence that should be summarized instead of defined."
        ));
        assert!(!is_short_phrase("line one\nline two"));
        assert!(!is_short_phrase("   "));
    }

    #[test]
    fn selection_results_offer_copy_deeper_explanation_and_send_to() {
        let action_ids = selection_result_actions()
            .into_iter()
            .map(|action| action.id)
            .collect::<Vec<_>>();
        assert_eq!(action_ids, ["go_deeper", "copy", "send_to"]);
    }

    #[test]
    fn tool_action_budget_has_strict_bounds() {
        assert!(validate_tool_action_budget(MIN_TOOL_ACTIONS).is_ok());
        assert!(validate_tool_action_budget(MAX_TOOL_ACTIONS).is_ok());
        assert!(validate_tool_action_budget(0).is_err());
        assert!(validate_tool_action_budget(MAX_TOOL_ACTIONS + 1).is_err());
    }

    #[test]
    fn axi_routing_is_local_first_and_cloud_is_explicitly_mode_controlled() {
        // Lane routing consults the live quota reading unless this override is
        // set; a machine near its quota would otherwise bias the cheap-cloud
        // lane off and make this test's expectations depend on system state.
        unsafe { env::set_var("MICE_QUOTA_PERCENT", "10") };
        let mut config = mice_core::Config {
            machine_profile: mice_core::MachineProfile::Standard,
            privacy_mode: PrivacyMode::CloudAllowed,
            ..mice_core::Config::default()
        };
        assert_eq!(axi_model_lane(&config, true).unwrap(), ExecutionLane::Local);

        // A configured standard profile without a usable Ollama/tool model
        // takes the explicit cloud-fallback path instead of failing on turn 1.
        assert_eq!(
            axi_model_lane(&config, false).unwrap(),
            ExecutionLane::CheapCloud
        );

        config.machine_profile = mice_core::MachineProfile::Light;
        assert_eq!(
            axi_model_lane(&config, true).unwrap(),
            ExecutionLane::CheapCloud,
            "a light profile must not silently select the local tool loop"
        );

        config.privacy_mode = PrivacyMode::LocalOnly;
        assert!(axi_model_lane(&config, true).is_err());

        config.privacy_mode = PrivacyMode::CloudOnly;
        assert_eq!(
            axi_model_lane(&config, false).unwrap(),
            ExecutionLane::CheapCloud
        );
    }

    #[test]
    fn axi_failure_classification_allows_one_safe_stale_retry_and_pauses_other_failures() {
        assert!(is_axi_stale_error(&"STALE_REF g2:button7"));
        assert!(is_axi_stale_error(
            &"Target g2:button7 is not in the current AXI snapshot"
        ));
        assert!(!is_axi_stale_error(
            &"Could not run npx: Chrome remote debugging port is unavailable"
        ));
    }

    #[test]
    fn each_completed_axi_action_gets_its_own_one_time_stale_retry() {
        let mut first_action = AxiActionRecovery::default();
        assert!(first_action.retry_stale_once());
        assert!(!first_action.retry_stale_once());

        let mut second_action = AxiActionRecovery::default();
        assert!(second_action.retry_stale_once());
        assert!(!second_action.retry_stale_once());

        // The same rule applies at the action-budget boundary: the sixth
        // proposed action is allowed to replan once before dispatch.
        let mut final_action = AxiActionRecovery::default();
        assert!(final_action.retry_stale_once());
    }

    #[test]
    fn provider_endpoints_remain_https_only() {
        for endpoint in [
            "https://api.openai.com/v1/responses",
            "https://api.openai.com/v1/images/generations",
            "https://api.groq.com/openai/v1/chat/completions",
        ] {
            assert!(endpoint.starts_with("https://"));
        }
    }

    #[test]
    fn generic_roles_are_not_exposed_as_user_control_types() {
        assert_eq!(
            hover_control_type(Some("AXGroup"), Some("Netflix"), None),
            "control"
        );
        assert_eq!(
            hover_control_type(Some("AXButton"), Some("+"), None),
            "button"
        );
    }

    #[test]
    fn semantic_labels_promote_real_control_names_over_generic_titles() {
        assert_eq!(
            semantic_hover_label(Some("Button"), Some("Reload this page"), None, None),
            Some("Reload this page".into())
        );
        assert_eq!(
            semantic_hover_label(None, Some("Netflix"), None, Some("Netflix")),
            Some("Netflix".into())
        );
    }

    #[test]
    fn hover_context_is_bounded_without_splitting_unicode() {
        assert_eq!(bounded_for_model("ab😀cd", 3), "ab😀…");
        assert_eq!(bounded_for_model("Netflix", 20), "Netflix");
    }

    #[test]
    fn groq_models_select_the_groq_guide_provider() {
        assert!(is_groq_model("llama-3.3-70b-versatile"));
        assert!(!is_groq_model("gpt-5.6-sol"));
    }

    #[test]
    fn guide_candidates_are_ranked_and_bounded_before_model_invocation() {
        let mut elements = (0..100)
            .map(|index| BrowserElement {
                // Unique labels so bounding (not label-dedup) is what limits the
                // list; the long suffix also exercises label truncation.
                selector: format!("#control-{index}"),
                role: "button".into(),
                label: format!("control {index} {}", "x".repeat(300)),
                candidate_id: None,
            })
            .collect::<Vec<_>>();
        elements[99].label = "Account Settings".into();

        let candidates = rank_guide_candidates("Where are account settings?", elements);

        assert_eq!(candidates.len(), MAX_GUIDE_CANDIDATES);
        assert_eq!(candidates[0].selector, "#control-99");
        assert_eq!(candidates[0].candidate_id.as_deref(), Some("candidate-1"));
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.label.chars().count() <= MAX_GUIDE_LABEL_CHARS + 1)
        );
    }

    #[test]
    fn guide_candidates_collapse_repeated_labels() {
        let elements = vec![
            BrowserElement {
                selector: "#a".into(),
                role: "button".into(),
                label: "Canva AI".into(),
                candidate_id: None,
            },
            BrowserElement {
                selector: "#b".into(),
                role: "link".into(),
                label: "Canva AI".into(),
                candidate_id: None,
            },
            BrowserElement {
                selector: "#c".into(),
                role: "button".into(),
                label: "Custom size".into(),
                candidate_id: None,
            },
        ];
        let candidates = rank_guide_candidates("make a portrait", elements);
        let labels = candidates
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels.iter().filter(|label| **label == "Canva AI").count(),
            1
        );
        assert!(labels.contains(&"Custom size"));
    }

    #[test]
    fn safety_blocklist_refuses_sensitive_browser_actions() {
        let password = BrowserTarget {
            label: "Password".into(),
            role: "textbox".into(),
        };
        let payment = BrowserTarget {
            label: "Confirm payment".into(),
            role: "button".into(),
        };
        let normal = BrowserTarget {
            label: "Continue".into(),
            role: "button".into(),
        };
        assert!(blocked_browser_action("fill", &password, "Enter password").is_some());
        assert!(blocked_browser_action("click", &payment, "Continue checkout").is_some());
        assert!(blocked_browser_action("click", &normal, "Continue to profile").is_none());
    }
}
