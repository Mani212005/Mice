use std::{
    collections::HashSet,
    env,
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mice_core::{action_instruction, clipboard_contents, config_path, load_config, save_config};
use mice_ipc::{
    AgentCommand, Capabilities, HoverCaptured, InitializeParams, RpcRequest, read_frame,
    write_frame,
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
    let result = match env::args().nth(1).as_deref() {
        Some("status") => status(),
        Some("doctor") => doctor(),
        Some("settings") => settings(),
        Some("actions") => actions(),
        Some("browser-bridge") => browser_bridge(),
        Some("start") => start(),
        Some("route") => route_preview(),
        Some("ask") => ask(),
        _ => usage(),
    };
    if let Err(error) = result {
        eprintln!("mice: {error}");
        std::process::exit(1);
    }
}

fn usage() -> Result<(), Box<dyn std::error::Error>> {
    println!("Usage: mice <start|stop|status|doctor|settings|actions|browser-bridge|route|ask>");
    println!("       mice ask [--action <preset>] <instruction>");
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
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

const MAX_GUIDE_CANDIDATES: usize = 80;
const MAX_GUIDE_LABEL_CHARS: usize = 180;
const MAX_GUIDE_ROLE_CHARS: usize = 80;
const MAX_GUIDE_SELECTOR_CHARS: usize = 1_024;

#[derive(Debug, Deserialize)]
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
}

fn browser_bridge() -> Result<(), Box<dyn std::error::Error>> {
    let token = env::var("MICE_BROWSER_BRIDGE_TOKEN")
        .map_err(|_| "MICE_BROWSER_BRIDGE_TOKEN must be set before starting the browser bridge")?;
    let config = config()?;
    let listener = TcpListener::bind("127.0.0.1:9417")?;
    println!("MICE browser bridge listening on http://127.0.0.1:9417");
    println!(
        "Load browser-ext as an unpacked extension and enter the same bridge token in its options."
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(error) = handle_browser_connection(stream, &token, &config) {
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
    if request_line != "POST /guide HTTP/1.1" || supplied_token != token {
        return write_http_json(
            &mut stream,
            401,
            &serde_json::json!({"error": "Unauthorized."}),
        );
    }
    let guide_request: BrowserGuideRequest = serde_json::from_slice(&request[header_end + 4..])?;
    match guide_browser_request(config, guide_request) {
        Ok(response) => write_http_json(&mut stream, 200, &response),
        Err(error) => write_http_json(
            &mut stream,
            422,
            &serde_json::json!({"error": error.to_string()}),
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
    match start_agent(&config.gesture.trigger) {
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
            KeyCode::Up | KeyCode::Char('k') => selected = selected.checked_sub(1).unwrap_or(4),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % 5,
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
    let mut agent = start_agent(&config.gesture.trigger)?;
    println!(
        "MICE is running with {} agent (overlay={}). Press Ctrl-C to stop.",
        agent.platform, agent.capabilities.overlay
    );
    println!("=== MICE Keyboard Gesture Loop ===");
    println!(
        "Press Control + Shift + Space to capture a region, or hold Control while hovering to explain a control."
    );

    while let Ok(msg) = read_frame::<mice_ipc::RpcNotification>(&mut agent.reader) {
        if msg.method == "selection.captured" {
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

fn start_agent(gesture_trigger: &str) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let agent = agent_path()?;
    if !agent.exists() {
        return Err(format!(
            "macOS agent has not been built yet at {}; run `swift build` in agent-macos first",
            agent.display()
        )
        .into());
    }
    let mut child = Command::new(agent)
        .env("MICE_GESTURE_TRIGGER", gesture_trigger)
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
    let mut agent = start_agent(&config.gesture.trigger)?;
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
                selector: format!("#control-{index}"),
                role: "button".into(),
                label: "x".repeat(300),
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
}
