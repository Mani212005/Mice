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
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mice_core::{
    AgentAction, AgentDecision, AgentLoop, AgentLoopState, AgentMode, GoalSession, GoalState,
    action_instruction, clipboard_contents, config_path, load_config, save_config,
};
use mice_ipc::{
    AgentCommand, Capabilities, HoverCaptured, InitializeParams, PromptSubmitted, RpcRequest,
    SelectionAction, SelectionText, read_frame, write_frame,
};
use mice_providers::{Action, Artifacts, ModelPreferences, RouteRequest, route};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::{Deserialize, Serialize};

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
        Some("browser-bridge") => browser_bridge(),
        Some("native-host") => native_host(),
        Some("setup-browser") => setup_browser(),
        Some("autopilot") => autopilot(),
        Some("start") => start(),
        Some("route") => route_preview(),
        Some("ask") => ask(),
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
        "Usage: mice <start|stop|status|doctor|settings|actions|browser-bridge|setup-browser|autopilot|route|ask>"
    );
    println!("       mice ask [--action <preset>] <instruction>");
    println!("       mice autopilot <goal>");
    Ok(())
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
    selector: String,
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

const AUTOPILOT_ACTION_BUDGET: usize = 15;
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
            Some("autopilot.start") => {
                let goal = message["goal"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if goal.is_empty() {
                    write_frame(
                        &mut stream,
                        &serde_json::json!({"type":"autopilot.status", "text":"Enter a goal before starting autopilot.", "done":true}),
                    )?;
                    continue;
                }
                if config.privacy_mode == mice_providers::PrivacyMode::LocalOnly {
                    write_frame(
                        &mut stream,
                        &serde_json::json!({"type":"autopilot.status", "text":"Autopilot requires cloud access. Change privacy mode first.", "done":true}),
                    )?;
                    continue;
                }
                let session_id = format!(
                    "autopilot-{}",
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
                );
                let writer = Arc::new(Mutex::new(stream.try_clone()?));
                let directive = BrowserGoalDirective {
                    session_id: session_id.clone(),
                    instruction: goal.clone(),
                };
                let chrome_connected = if let Ok(mut state) = bridge.lock() {
                    state.control_client = Some(writer);
                    state.directive = Some(directive.clone());
                    state.autopilot = Some(AutopilotRun {
                        session_id,
                        loop_state: AgentLoop::new(
                            goal,
                            AgentMode::Autopilot,
                            AUTOPILOT_ACTION_BUDGET,
                        ),
                        started_at: Instant::now(),
                        awaiting_page_change: false,
                        pending_snapshot: None,
                        action_deadline: None,
                        stuck_turns: 0,
                        last_observed_url: None,
                        in_flight: false,
                    });
                    state.client.is_some()
                } else {
                    false
                };
                // The goal.step below reaches the browser only if the Chrome
                // extension is connected to this daemon. If it is not, say so
                // clearly instead of pretending to observe — the pending
                // directive is replayed automatically when the extension
                // connects (see the bridge.hello handler).
                if chrome_connected {
                    autopilot_status(
                        &bridge,
                        "Autopilot started. Observing the current page.",
                        false,
                    );
                    native_bridge_send(
                        &bridge,
                        &serde_json::json!({"type":"goal.step", "directive":directive}),
                    );
                } else {
                    autopilot_status(
                        &bridge,
                        "Autopilot is ready, but Chrome is not connected yet. Keep `mice start` running with the MICE extension loaded and Chrome open — I will begin automatically when it connects.",
                        false,
                    );
                }
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
                                selector: response.selector.clone(),
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
            selector: selected.selector.clone(),
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
    let goal = env::args().skip(2).collect::<Vec<_>>().join(" ");
    if goal.trim().is_empty() {
        return Err(
            "Provide a goal, for example: mice autopilot \"search Canva and open a portrait\""
                .into(),
        );
    }
    println!(
        "MICE will use the configured cloud model to click and type for this goal. It never enters passwords, one-time codes, payment data, or final submissions."
    );
    println!("Goal: {goal}");
    print!("Start autopilot? [y/N] ");
    std::io::stdout().flush()?;
    let mut consent = String::new();
    std::io::stdin().read_line(&mut consent)?;
    if !matches!(consent.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        println!("Autopilot was not started.");
        return Ok(());
    }

    let mut stream = UnixStream::connect(bridge_socket_path()?)
        .map_err(|_| "Start the MICE daemon first: `cargo run -p mice-cli -- start`.")?;
    write_frame(
        &mut stream,
        &serde_json::json!({"type":"autopilot.start", "goal":goal}),
    )?;
    while let Ok(status) = read_frame::<serde_json::Value>(&mut stream) {
        if status["type"] != "autopilot.status" {
            continue;
        }
        if let Some(text) = status["text"].as_str() {
            println!("[MICE autopilot] {text}");
        }
        if status["done"].as_bool().unwrap_or(false) {
            break;
        }
    }
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

fn call_openai_guide(
    instruction: &str,
    dom_snapshot: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for browser guide-me")?;
    let payload = mice_providers::openai_guide_payload(instruction, dom_snapshot).to_string();
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.openai.com/v1/responses",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "OpenAI Responses API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.groq.com/openai/v1/chat/completions",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Groq guide API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.openai.com/v1/responses",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "OpenAI Goal Guide API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.groq.com/openai/v1/chat/completions",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Groq Goal Guide API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.openai.com/v1/responses",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "OpenAI autopilot API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.groq.com/openai/v1/chat/completions",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Groq autopilot API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
}

fn enabled(value: bool) -> &'static str {
    if value { "available" } else { "unavailable" }
}

fn doctor() -> Result<(), Box<dyn std::error::Error>> {
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
            KeyCode::Up | KeyCode::Char('k') => selected = selected.checked_sub(1).unwrap_or(9),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % 10,
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
            &mut config.gesture.goal_trigger,
            &["ctrl+alt+space"],
            forward,
        ),
        8 => cycle_value(
            &mut config.autopilot.persona,
            &["patient", "concise", "playful"],
            forward,
        ),
        9 => config.autopilot.careful_mode = !config.autopilot.careful_mode,
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
        "Capture: Ctrl+Shift+Space. Hover: hold Control. Select text, then Ctrl double-tap to summarize or Ctrl+Option+I for an infographic."
    );

    let mut goal_sessions = HashMap::<String, GoalSession>::new();
    let mut goal_plans = HashMap::<String, GoalPlanResult>::new();
    let mut active_guides = HashMap::<String, ActiveGuide>::new();
    let mut selection_cache: Option<SelectionCache> = None;
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
                    selection_cache = Some(SelectionCache {
                        session_id,
                        text,
                        response,
                    });
                }
                Ok(None) => {}
                Err(error) => eprintln!("MICE selection action failed: {error}"),
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

            let mut response = String::new();
            let result = if selected.locality == mice_providers::Locality::Local {
                stream_ollama(
                    selected.id,
                    &request.instruction,
                    text.as_deref(),
                    |chunk| {
                        print!("{chunk}");
                        let _ = std::io::stdout().flush();
                        response.push_str(chunk);
                        send_command(
                            &mut agent.stdin,
                            AgentCommand::OverlayAppendResult {
                                chunk: chunk.into(),
                            },
                        )
                    },
                )
            } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
                stream_groq(
                    selected.id,
                    &request.instruction,
                    text.as_deref(),
                    |chunk| {
                        print!("{chunk}");
                        let _ = std::io::stdout().flush();
                        response.push_str(chunk);
                        send_command(
                            &mut agent.stdin,
                            AgentCommand::OverlayAppendResult {
                                chunk: chunk.into(),
                            },
                        )
                    },
                )
            } else {
                stream_openai(
                    selected.id,
                    &request.instruction,
                    text.as_deref(),
                    |chunk| {
                        print!("{chunk}");
                        let _ = std::io::stdout().flush();
                        response.push_str(chunk);
                        send_command(
                            &mut agent.stdin,
                            AgentCommand::OverlayAppendResult {
                                chunk: chunk.into(),
                            },
                        )
                    },
                )
            };

            println!(); // Print new line after stream ends

            match result {
                Ok(()) => {
                    if !response.is_empty() {
                        send_command(&mut agent.stdin, clipboard_command(&response))?;
                    }
                    send_command(
                        &mut agent.stdin,
                        AgentCommand::OverlayFinishResult { text: None },
                    )?;
                }
                Err(error) => {
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
fn selection_result_actions() -> Vec<mice_ipc::OverlayAction> {
    vec![
        mice_ipc::OverlayAction {
            id: "go_deeper".into(),
            label: "Go Deeper".into(),
        },
        mice_ipc::OverlayAction {
            id: "copy".into(),
            label: "Copy".into(),
        },
    ]
}

/// Stream a text action to the overlay through the appropriate provider lane and
/// return the full response. Shared by the summarize and go-deeper paths.
fn stream_selected(
    writer: &mut ChildStdin,
    selected: &mice_providers::ModelDescriptor,
    instruction: &str,
    text: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut response = String::new();
    let result = if selected.locality == mice_providers::Locality::Local {
        stream_ollama(selected.id, instruction, Some(text), |chunk| {
            response.push_str(chunk);
            send_command(
                writer,
                AgentCommand::OverlayAppendResult {
                    chunk: chunk.into(),
                },
            )
        })
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(selected.id, instruction, Some(text), |chunk| {
            response.push_str(chunk);
            send_command(
                writer,
                AgentCommand::OverlayAppendResult {
                    chunk: chunk.into(),
                },
            )
        })
    } else {
        stream_openai(selected.id, instruction, Some(text), |chunk| {
            response.push_str(chunk);
            send_command(
                writer,
                AgentCommand::OverlayAppendResult {
                    chunk: chunk.into(),
                },
            )
        })
    };
    result?;
    Ok(response)
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
    let selected = route(&request)?.model;
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: "Going deeper…".into(),
        },
    )?;
    let response = stream_selected(writer, &selected, &request.instruction, text)?;
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
            let response = run_go_deeper(writer, config, session_id, &text)?;
            if let Some(entry) = cache.as_mut() {
                entry.response = response;
            }
        }
        _ => {}
    }
    Ok(())
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
    let request = RouteRequest {
        artifacts: Artifacts {
            text: Some(selection.text.clone()),
            ..Default::default()
        },
        instruction: action_instruction(action, ""),
        action: Some(action),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    let selected = route(&request)?.model;
    let status = match action {
        Action::Summarize => "Summarizing selection…",
        Action::Define => "Defining…",
        Action::Image => "Creating infographic…",
        _ => unreachable!("selection actions are constrained above"),
    };
    send_command(
        writer,
        AgentCommand::OverlayShow {
            text: status.into(),
        },
    )?;

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

    match stream_selected(writer, &selected, &request.instruction, &selection.text) {
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
            browser_capable: is_browser_step(step) && !step.sensitive,
        },
    )
}

fn handle_guide_control(
    writer: &mut ChildStdin,
    guides: &mut HashMap<String, ActiveGuide>,
    browser_goal_directive: &NativeBridge,
    session_id: &str,
    action: &str,
    value: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if action == "do-it" || action == "do-it-fill" {
        let guide = guides
            .get(session_id)
            .ok_or("No active guide was found for this session.")?;
        let step = guide
            .steps
            .get(guide.current_step)
            .ok_or("The active guide has no current step.")?;
        if step.sensitive || !is_browser_step(step) {
            return send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some("This one's yours — MICE will only highlight this step.".into()),
                },
            );
        }
        let target = browser_goal_directive
            .lock()
            .ok()
            .and_then(|state| state.targets.get(session_id).cloned());
        let Some(target) = target else {
            return send_command(writer, AgentCommand::OverlayFinishResult { text: Some("No verified target is available. Reopen this step to highlight the current page first.".into()) });
        };
        let kind = if action == "do-it-fill" {
            "fill"
        } else {
            "click"
        };
        if kind == "fill" && value.unwrap_or_default().is_empty() {
            return send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some("Enter text before confirming a type action.".into()),
                },
            );
        }
        if let Some(reason) = blocked_browser_action(kind, &target, &step.instruction) {
            return send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some(format!(
                        "This one's yours — MICE highlights it but won't act: {reason}"
                    )),
                },
            );
        }
        native_bridge_send(
            browser_goal_directive,
            &serde_json::json!({"type":"browser.act", "sessionId":session_id, "action":kind, "selector":target.selector, "value":value}),
        );
        println!("[MICE act] confirmed {kind} on '{}'", target.label);
        return send_command(
            writer,
            AgentCommand::OverlayFinishResult {
                text: Some(format!(
                    "Action sent: {kind} {}. Check the page, then choose Next when ready.",
                    target.label
                )),
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
    let mut response = String::new();
    let result = if selected.locality == mice_providers::Locality::Local {
        stream_ollama(selected.id, &request.instruction, Some(&context), |chunk| {
            response.push_str(chunk);
            send_command(
                writer,
                AgentCommand::OverlayAppendResult {
                    chunk: chunk.into(),
                },
            )
        })
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(selected.id, &request.instruction, Some(&context), |chunk| {
            response.push_str(chunk);
            send_command(
                writer,
                AgentCommand::OverlayAppendResult {
                    chunk: chunk.into(),
                },
            )
        })
    } else {
        stream_openai(selected.id, &request.instruction, Some(&context), |chunk| {
            response.push_str(chunk);
            send_command(
                writer,
                AgentCommand::OverlayAppendResult {
                    chunk: chunk.into(),
                },
            )
        })
    };
    match result {
        Ok(()) => send_command(writer, AgentCommand::OverlayFinishResult { text: None }),
        Err(error) => {
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
    start_agent_with_mode(gesture, false)
}

fn start_agent_with_mode(
    gesture: &mice_core::GestureConfig,
    autopilot_active: bool,
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
        .env(
            "MICE_AUTOPILOT_ACTIVE",
            if autopilot_active { "1" } else { "0" },
        )
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

    // Cargo compiles this source inside `crates/mice-cli`; use that stable
    // workspace location instead of the caller's current directory. Packaging
    // can supply its agent location through MICE_MAC_AGENT_PATH later.
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
    let mut agent = start_agent(&config.gesture)?;
    send_command(
        &mut agent.stdin,
        AgentCommand::OverlayShow {
            text: "MICE is thinking…".into(),
        },
    )?;
    println!("[{}]", selected.id);
    let mut response = String::new();
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
    let result = if selected.locality == mice_providers::Locality::Local {
        stream_ollama(
            selected.id,
            &request.instruction,
            text.as_deref(),
            |chunk| {
                print!("{chunk}");
                let _ = std::io::stdout().flush();
                response.push_str(chunk);
                send_command(
                    &mut agent.stdin,
                    AgentCommand::OverlayAppendResult {
                        chunk: chunk.into(),
                    },
                )
            },
        )
    } else if selected.id.starts_with("llama-") || selected.id.starts_with("mixtral-") {
        stream_groq(
            selected.id,
            &request.instruction,
            text.as_deref(),
            |chunk| {
                print!("{chunk}");
                let _ = std::io::stdout().flush();
                response.push_str(chunk);
                send_command(
                    &mut agent.stdin,
                    AgentCommand::OverlayAppendResult {
                        chunk: chunk.into(),
                    },
                )
            },
        )
    } else {
        stream_openai(
            selected.id,
            &request.instruction,
            text.as_deref(),
            |chunk| {
                print!("{chunk}");
                let _ = std::io::stdout().flush();
                response.push_str(chunk);
                send_command(
                    &mut agent.stdin,
                    AgentCommand::OverlayAppendResult {
                        chunk: chunk.into(),
                    },
                )
            },
        )
    };
    match result {
        Ok(()) => {
            if !response.is_empty() {
                send_command(&mut agent.stdin, clipboard_command(&response))?;
            }
            send_command(
                &mut agent.stdin,
                AgentCommand::OverlayFinishResult { text: None },
            )
        }
        Err(error) => {
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
    let contents = clipboard_contents(text);
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
    // Supplying a large capture as an argv item fails with macOS E2BIG. Ollama
    // also accepts prompts on standard input, which has no command-line limit.
    let request = prompt(instruction, text);
    let mut child = Command::new("ollama")
        .args(["run", model])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("Ollama stdin was unavailable")?;
    stdin.write_all(request.as_bytes())?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or("Ollama stdout was unavailable")?;
    let mut ansi_stripper = AnsiStripper::default();
    for line in BufReader::new(stdout).lines() {
        let chunk = ansi_stripper.strip(&(line? + "\n"));
        if !chunk.is_empty() {
            on_chunk(&chunk)?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("Ollama failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
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

#[derive(Default)]
struct AnsiStripper {
    state: AnsiState,
}

#[derive(Default)]
enum AnsiState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

impl AnsiStripper {
    fn strip(&mut self, input: &str) -> String {
        let mut output = String::with_capacity(input.len());
        for character in input.chars() {
            match self.state {
                AnsiState::Text => match character {
                    '\u{1b}' => self.state = AnsiState::Escape,
                    '\r' => {}
                    _ => output.push(character),
                },
                AnsiState::Escape => match character {
                    '[' => self.state = AnsiState::Csi,
                    ']' => self.state = AnsiState::Osc,
                    _ => self.state = AnsiState::Text,
                },
                AnsiState::Csi => {
                    if ('@'..='~').contains(&character) {
                        self.state = AnsiState::Text;
                    }
                }
                AnsiState::Osc => match character {
                    '\u{7}' => self.state = AnsiState::Text,
                    '\u{1b}' => self.state = AnsiState::OscEscape,
                    _ => {}
                },
                AnsiState::OscEscape => {
                    self.state = if character == '\\' {
                        AnsiState::Text
                    } else {
                        AnsiState::Osc
                    };
                }
            }
        }
        output
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
    let mut child = Command::new("curl")
        .args([
            "--no-buffer",
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.openai.com/v1/responses",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("OpenAI stdout was unavailable")?;
    for line in BufReader::new(stdout).lines() {
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
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "OpenAI Responses API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

fn generate_openai_image(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for image generation")?;
    let payload = mice_providers::openai_image_generation_payload(prompt).to_string();
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.openai.com/v1/images/generations",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "OpenAI Images API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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

    let mut child = Command::new("curl")
        .args([
            "--no-buffer",
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--request",
            "POST",
            "https://api.groq.com/openai/v1/chat/completions",
            "--header",
            "Content-Type: application/json",
            "--header",
            &format!("Authorization: Bearer {api_key}"),
            "--data",
            &payload,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("Groq stdout was unavailable")?;
    for line in BufReader::new(stdout).lines() {
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
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "Groq API failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
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
    fn ansi_escape_sequences_are_not_forwarded_to_the_overlay() {
        let mut stripper = AnsiStripper::default();
        assert_eq!(
            stripper.strip("hello\u{1b}[3D\u{1b}[K world\r\n"),
            "hello world\n"
        );
    }

    #[test]
    fn ansi_sequences_split_across_stream_chunks_are_not_leaked() {
        let mut stripper = AnsiStripper::default();
        assert_eq!(stripper.strip("hello\u{1b}"), "hello");
        assert_eq!(stripper.strip("[6D\u{1b}"), "");
        assert_eq!(stripper.strip("[K world"), " world");
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
            selector: "#password".into(),
            label: "Password".into(),
            role: "textbox".into(),
        };
        let payment = BrowserTarget {
            selector: "#pay".into(),
            label: "Confirm payment".into(),
            role: "button".into(),
        };
        let normal = BrowserTarget {
            selector: "#continue".into(),
            label: "Continue".into(),
            role: "button".into(),
        };
        assert!(blocked_browser_action("fill", &password, "Enter password").is_some());
        assert!(blocked_browser_action("click", &payment, "Continue checkout").is_some());
        assert!(blocked_browser_action("click", &normal, "Continue to profile").is_none());
    }
}
