mod coordination;
mod filing;
mod mcp_client;
mod memory;
mod mission;
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
    path::{Path, PathBuf},
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
    LOCAL_SUMMARY_CHUNK_TOKENS, LOCAL_SUMMARY_REDUCE_TOKENS, MachineProfile, PaletteIntent,
    ScheduleAction, ScheduledTask, SmartCopyPlan, ToolDecision, action_instruction,
    apply_preferences, chunk_summary_instruction, clipboard_contents, config_path, estimate_tokens,
    load_config, looks_like_code, parse_markdown_table, parse_palette_intent, parse_schedule_time,
    reduce_summary_instruction, route_execution_lane, save_config, selection_summary_instruction,
    smart_copy_chunks, smart_copy_clean_instruction, smart_copy_plan, smart_copy_preserves_links,
    smart_copy_preserves_visible_text, smart_copy_table_instruction, structural_summary_chunks,
    summary_reduce_batches, table_clipboard_contents,
};
use mice_ipc::{
    AgentCommand, Capabilities, ClipboardCaptured, HoverCaptured, InitializeParams,
    PaletteSubmitted, PromptSubmitted, RpcRequest, SelectionAction, SelectionText, read_frame,
    write_frame,
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
    let packaged_app_launch = command.is_none()
        && env::current_exe()
            .ok()
            .as_deref()
            .and_then(packaged_app_root)
            .is_some();
    let chrome_native_host = command
        .as_deref()
        .is_some_and(|argument| argument.starts_with("chrome-extension://"))
        || (command.is_none() && !std::io::stdin().is_terminal() && !packaged_app_launch);
    let result = match command.as_deref() {
        None if !chrome_native_host => launch(),
        Some("help") | Some("--help") | Some("-h") => usage(),
        Some("install") => install(),
        Some("home") => home(),
        Some("setup") => setup(),
        Some("connect") => connect(),
        Some("disconnect") => disconnect(),
        Some("integrations") => integrations(),
        Some("status") => status(),
        Some("doctor") => doctor(),
        Some("settings") => settings(),
        Some("keys") | Some("key") => keys(),
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
        Some("history") => history(),
        Some("plans") => plans(),
        Some("tidy") => tidy::tidy(),
        Some("file") => filing::file_cmd(),
        Some("mcp") => mcp_cmd(),
        Some("mcp-server") => mcp_server(),
        Some("schedule") => schedule_cmd(),
        Some("team") => team(),
        Some("mission") => mission::command(&env::args().skip(2).collect::<Vec<_>>()),
        _ if chrome_native_host => native_host(),
        _ => usage(),
    };
    if let Err(error) = result {
        eprintln!("mice: {error}");
        std::process::exit(1);
    }
}

fn usage() -> Result<(), Box<dyn std::error::Error>> {
    println!("MICE — native, privacy-aware desktop assistance");
    println!("Usage: mice [command]");
    println!(
        "\nApp\n  mice                     Start/reuse MICE and open MICE Home\n  mice start | stop | home | setup | install | status | doctor | settings\n  mice keys <set|status|delete> [groq|openai]"
    );
    println!(
        "\nAsk and screen\n  mice ask [--action <preset>] <instruction>\n  mice see [--display|--sheet] <question>\n  mice history [query] | mice history --clear | mice plans\n  mice actions | route"
    );
    println!(
        "\nGoals, browser, and files\n  mice autopilot [--engine axi] <goal>\n  mice do [--model <model>] [--max-actions <n>] [--session <name>] <goal>\n  mice tidy [--apply] [--no-label] [folder] | mice tidy --undo\n  mice file --add-root <folder> | --finder | <path>"
    );
    println!(
        "\nIntegrations\n  mice connect <codex|claude|all> [--yes]\n  mice disconnect <codex|claude|all> [--yes]\n  mice integrations\n  mice mcp-server | mice mcp <list|call>"
    );
    println!(
        "\nAdvanced\n  mice team [status|risks]\n  mice mission plan <plan-file> --agents <codex,claude,antigravity> [--planner auto|markdown] [--allow-cloud] [--review|--dry-run|--launch --yes]\n  mice mission status|watch <plan-file>\n  mice tools | bench-tools | savings | advertise | setup-browser"
    );
    Ok(())
}

/// Coordination Mesh P0 is deliberately a read-only snapshot. It discovers
/// the current repository's existing worktrees and writes only metadata to
/// owner-only MICE application support storage.
fn team() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let mode = match arguments.as_slice() {
        [] => "status",
        [value] if value == "status" => "status",
        [value] if value == "risks" => "risks",
        _ => return Err("Usage: mice team [status|risks]".into()),
    };
    let cwd = env::current_dir()?;
    let snapshot = coordination::discover(&cwd).map_err(|error| {
        format!("Coordination requires a Git worktree. Run this from a repository: {error}")
    })?;
    let root = coordination::SnapshotStore::default_path().ok_or("HOME is not set")?;
    let store = coordination::SnapshotStore::at(root)?;
    let saved = store.record(&snapshot)?;
    println!("{}", coordination::render_status(&snapshot, &saved));
    if mode == "risks" {
        println!();
        println!(
            "{}",
            coordination::render_risks(&coordination::merge_risks(&snapshot))
        );
    }
    Ok(())
}

const MICE_HOME_TEXT: &str = "MICE Home\n\nAsk & explain\n• Ctrl+Shift+Space — open the command palette\n• Hold Control over a control — explain it\n• Ctrl double-tap after selecting text — quick recap\n• Ctrl+Option+I — make an infographic\n• Cmd-C, then Ctrl+Option+C — Smart Copy\n\nGuide & act\n• Ctrl+Option+Space — plan a goal; you review before starting\n• `mice autopilot <goal>` — browser actions, one confirmation at a time\n\nFiles & screen\n• `mice see <question>` — ask about your screen\n• `mice tidy` — review folder cleanup suggestions\n• `mice file <path>` — file an item into a registered root\n\nPrivacy & integrations\n• `mice settings` — choose cloud allowed, cloud only, or local only\n• `mice connect all` — add MICE's private local tools to Codex and Claude\n• Esc hides MICE; `mice stop` stops the background service.";

fn home_text(config: &mice_core::Config) -> String {
    let plans = recent_plan_preview();
    format!(
        "MICE Home\nPrivacy mode: {}\nLocal model: {}\nCloud model: {}\nRecent plans:\n{}\n\n{}",
        privacy_mode_name(config.privacy_mode),
        config.local_model,
        config.cloud_model,
        plans,
        MICE_HOME_TEXT
            .strip_prefix("MICE Home\n\n")
            .unwrap_or(MICE_HOME_TEXT)
    )
}

/// Home is deliberately a compact, local view. The complete bounded plans
/// remain available through `mice plans`; this preview makes a saved plan
/// discoverable without turning the reference surface into a transcript.
fn recent_plan_preview() -> String {
    let Ok(history) = user_history() else {
        return "No saved plans yet.".into();
    };
    let Ok(events) = history.search(None) else {
        return "No saved plans yet.".into();
    };
    let plans = events
        .into_iter()
        .filter(|entry| entry.kind == memory::HistoryKind::GoalPlan)
        .take(2)
        .map(|entry| format!("• {}", bounded_for_model(&entry.question, 72)))
        .collect::<Vec<_>>();
    if plans.is_empty() {
        "No saved plans yet.".into()
    } else {
        plans.join("\n")
    }
}

/// The everyday entry point. It never ties up the caller's terminal: a
/// background daemon owns the gesture loop, while `mice start` remains useful
/// when a developer deliberately wants foreground diagnostics.
fn launch() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(path) = bridge_socket_path()
        && let Ok(mut stream) = UnixStream::connect(&path)
    {
        let mut verified = false;
        if write_frame(&mut stream, &serde_json::json!({"type":"palette.show"})).is_ok()
            && let Ok(resp) = read_frame::<serde_json::Value>(&mut stream)
            && resp["type"].as_str() == Some("palette.showing")
        {
            verified = true;
        }
        if verified {
            println!("MICE is running. Command palette is open.");
            return Ok(());
        }
        let _ = std::fs::remove_file(&path);
    }
    let executable = env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("start")
        .env("MICE_OPEN_HOME", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Local Only may need to start Ollama and pull the approved first model,
    // which legitimately takes minutes. Do not claim failure after eight
    // seconds and invite a second competing bootstrap.
    let startup_budget = if config()?.privacy_mode == PrivacyMode::LocalOnly {
        Duration::from_secs(10 * 60)
    } else {
        Duration::from_secs(30)
    };
    let deadline = Instant::now() + startup_budget;
    while Instant::now() < deadline {
        if UnixStream::connect(bridge_socket_path()?).is_ok() {
            println!("MICE is running. MICE Home is open.");
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "MICE stopped during startup ({status}). Run `mice start` for diagnostics."
            )
            .into());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("MICE is still preparing Local Only startup. Keep this process running or use `mice start` for diagnostics; do not start a second daemon.".into())
}

/// Open MICE Home without enabling any global input capture. This display-only
/// helper exits with the native panel, so it can safely be launched beside an
/// already-running daemon.
fn home() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    if let Ok(path) = bridge_socket_path()
        && let Ok(mut stream) = UnixStream::connect(&path)
    {
        let mut verified = false;
        if write_frame(&mut stream, &serde_json::json!({"type":"home.show"})).is_ok()
            && let Ok(resp) = read_frame::<serde_json::Value>(&mut stream)
            && resp["type"].as_str() == Some("home.showing")
        {
            verified = true;
        }
        if verified {
            println!("MICE Home is open. Press Esc to hide it.");
            return Ok(());
        }
    }
    let mut agent = start_agent_home(&config.gesture, false)?;
    send_command(
        &mut agent.stdin,
        AgentCommand::HomeShow {
            text: home_text(&config),
        },
    )?;
    println!("MICE Home is open. Press Esc to hide it.");
    loop {
        thread::sleep(Duration::from_secs(1));
        if agent.child.try_wait()?.is_some() {
            break;
        }
    }
    Ok(())
}

/// First-run setup is deliberately explicit about permissions, while local
/// model setup is automatic only for the approved default model.
fn setup() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    // Setup is the explicit one-time preparation command. It makes the safe
    // default local lane ready even when the everyday routing mode is cloud
    // allowed, without changing the user's configured privacy mode.
    let mut local_setup = config.clone();
    local_setup.local_model = DEFAULT_AUTOMATIC_LOCAL_MODEL.into();
    ensure_local_model(&local_setup)?;
    println!("MICE setup complete. Run `mice status` to inspect macOS permissions.");
    Ok(())
}

fn packaged_app_root(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                symlink(std::fs::read_link(&from)?, &to)?;
            }
            #[cfg(not(unix))]
            {
                return Err("MICE cannot preserve bundle symlinks on this platform.".into());
            }
        } else if file_type.is_dir() {
            copy_directory(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Install only into paths owned by the current user. It never edits shell
/// profiles and refuses to overwrite an unrelated executable named `mice`.
fn install() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::fs::canonicalize(env::current_exe()?)?;
    let source_app = packaged_app_root(&executable).ok_or(
        "`mice install` must run from MICE.app. Build the package first with `scripts/package-macos.sh`.",
    )?;
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    let applications = PathBuf::from(&home).join("Applications");
    let destination = applications.join("MICE.app");
    if source_app == destination {
        println!("MICE is already installed at {}.", destination.display());
    } else {
        std::fs::create_dir_all(&applications)?;
        let staged = applications.join(format!(".MICE.app.installing-{}", std::process::id()));
        if staged.exists() {
            std::fs::remove_dir_all(&staged)?;
        }
        copy_directory(&source_app, &staged)?;
        let backup = applications.join(format!(".MICE.app.backup-{}", std::process::id()));
        if destination.exists() {
            std::fs::rename(&destination, &backup)?;
        }
        if let Err(error) = std::fs::rename(&staged, &destination) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, &destination);
            }
            return Err(error.into());
        }
        if backup.exists() {
            std::fs::remove_dir_all(backup)?;
        }
        println!("Installed MICE.app at {}.", destination.display());
    }
    let bin = PathBuf::from(&home).join(".local/bin");
    std::fs::create_dir_all(&bin)?;
    let launcher = bin.join("mice");
    let target = destination.join("Contents/MacOS/mice");
    if launcher.exists() || launcher.is_symlink() {
        let existing = std::fs::canonicalize(&launcher).ok();
        if existing.as_deref() != Some(target.as_path()) {
            return Err(format!(
                "Refusing to replace existing {}. Remove it yourself, then run `mice install` again.",
                launcher.display()
            )
            .into());
        }
        std::fs::remove_file(&launcher)?;
    }
    std::os::unix::fs::symlink(&target, &launcher)?;
    let in_path =
        env::var_os("PATH").is_some_and(|paths| env::split_paths(&paths).any(|path| path == bin));
    println!("Installed command launcher at {}.", launcher.display());
    if !in_path {
        println!("Add it to PATH once: export PATH=\"$HOME/.local/bin:$PATH\"");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Harness {
    Codex,
    Claude,
}

impl Harness {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

fn requested_harnesses(
    arguments: &[String],
) -> Result<(Vec<Harness>, bool), Box<dyn std::error::Error>> {
    let Some(target) = arguments.first().map(String::as_str) else {
        return Err("Usage: mice <connect|disconnect> <codex|claude|all> [--yes]".into());
    };
    let yes = arguments.iter().skip(1).any(|argument| argument == "--yes");
    if arguments.iter().skip(1).any(|argument| argument != "--yes") {
        return Err("Usage: mice <connect|disconnect> <codex|claude|all> [--yes]".into());
    }
    let harnesses = if target == "all" {
        vec![Harness::Codex, Harness::Claude]
    } else {
        vec![Harness::parse(target).ok_or("Choose codex, claude, or all.")?]
    };
    Ok((harnesses, yes))
}

fn confirm_change(summary: &str, yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("{summary}");
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err("Use --yes when running without an interactive terminal.".into());
    }
    print!("Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err("Cancelled; no integration settings changed.".into())
    }
}

fn harness_available(harness: Harness) -> bool {
    Command::new(harness.binary())
        .arg("mcp")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn packaged_mice_executable() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let executable = std::fs::canonicalize(env::current_exe()?)?;
    let Some(app) = packaged_app_root(&executable) else {
        return Err(
            "MICE integrations require the packaged app. Run `mice install` from MICE.app first."
                .into(),
        );
    };
    Ok(app.join("Contents/MacOS/mice"))
}

fn harness_mice_entry(harness: Harness) -> Option<String> {
    let output = Command::new(harness.binary())
        .args(["mcp", "get", "mice"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn harness_has_this_mice(harness: Harness, executable: &Path) -> bool {
    let canonical = executable
        .canonicalize()
        .unwrap_or_else(|_| executable.into());
    harness_mice_entry(harness).is_some_and(|entry| {
        entry.contains(&canonical.display().to_string()) && entry.contains("mcp-server")
    })
}

fn connect() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let (harnesses, yes) = requested_harnesses(&arguments)?;
    let missing = harnesses
        .iter()
        .copied()
        .filter(|harness| !harness_available(*harness))
        .map(|harness| harness.name())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!("Not installed or unavailable: {}.", missing.join(", ")).into());
    }
    let executable = packaged_mice_executable()?;
    let existing = harnesses
        .iter()
        .copied()
        .filter(|harness| harness_mice_entry(*harness).is_some())
        .map(|harness| harness.name())
        .collect::<Vec<_>>();
    if !existing.is_empty() {
        return Err(format!(
            "MICE is already registered for {}. Run `mice integrations` to inspect it; MICE will not replace an existing entry automatically.",
            existing.join(", ")
        )
        .into());
    }
    confirm_change(
        &format!(
            "Register MICE's local-only MCP server for {}.\nCommand: {} mcp-server\nNo repository files, cloud keys, or copied content are shared.",
            harnesses
                .iter()
                .map(|harness| harness.name())
                .collect::<Vec<_>>()
                .join(" and "),
            executable.display()
        ),
        yes,
    )?;
    let mut connected = Vec::new();
    for harness in harnesses {
        let mut command = Command::new(harness.binary());
        command.arg("mcp").arg("add");
        if matches!(harness, Harness::Claude) {
            command.args(["--scope", "user"]);
        }
        let status = command
            .arg("mice")
            .arg("--")
            .arg(&executable)
            .arg("mcp-server")
            .status()?;
        if !status.success() {
            // `connect all` is transactional from the user's perspective.
            // Roll back only entries we just created and can positively
            // identify as this executable.
            for prior in connected {
                if harness_has_this_mice(prior, &executable) {
                    let _ = Command::new(prior.binary())
                        .args(["mcp", "remove", "mice"])
                        .status();
                }
            }
            return Err(format!("{} rejected the MICE MCP registration.", harness.name()).into());
        }
        connected.push(harness);
        println!("Connected {}.", harness.name());
    }
    Ok(())
}

fn disconnect() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let (harnesses, yes) = requested_harnesses(&arguments)?;
    confirm_change(
        &format!(
            "Remove MICE's MCP entry from {}. This does not affect MICE.app or your files.",
            harnesses
                .iter()
                .map(|harness| harness.name())
                .collect::<Vec<_>>()
                .join(" and ")
        ),
        yes,
    )?;
    let executable = packaged_mice_executable()?;
    for harness in harnesses {
        if !harness_available(harness) {
            println!("{} is unavailable; nothing changed there.", harness.name());
            continue;
        }
        if !harness_has_this_mice(harness, &executable) {
            println!(
                "{} has no MICE entry owned by this installation; nothing changed.",
                harness.name()
            );
            continue;
        }
        let status = Command::new(harness.binary())
            .args(["mcp", "remove", "mice"])
            .status()?;
        if status.success() {
            println!("Disconnected {}.", harness.name());
        } else {
            println!("{} has no removable MICE entry.", harness.name());
        }
    }
    Ok(())
}

fn integrations() -> Result<(), Box<dyn std::error::Error>> {
    let executable = packaged_mice_executable().ok();
    for harness in [Harness::Codex, Harness::Claude] {
        let availability = if harness_available(harness) {
            "available"
        } else {
            "not installed"
        };
        let connection = if executable
            .as_ref()
            .is_some_and(|path| harness_has_this_mice(harness, path))
        {
            "connected"
        } else if harness_mice_entry(harness).is_some() {
            "mice entry belongs to another installation"
        } else {
            "not connected"
        };
        println!("{}: {availability}; {connection}", harness.name());
    }
    println!("MICE MCP tools always use the local Ollama model, never a cloud provider.");
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
    goal_sessions: Option<Arc<Mutex<HashMap<String, GoalSession>>>>,
    overlay_shown: bool,
    /// The browser bridge is also the bounded local handoff path for Mission
    /// Control lifecycle notices. Repeating the same state transition should
    /// never create a stack of identical native panels.
    last_mission_notification: Option<(String, Instant)>,
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

fn present_mission_notification(
    bridge: &NativeBridge,
    gesture: mice_core::GestureConfig,
    text: String,
) {
    let text = text
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(600)
        .collect::<String>();
    if text.trim().is_empty() {
        return;
    }
    let should_show = bridge.lock().ok().is_some_and(|mut state| {
        let repeated =
            state
                .last_mission_notification
                .as_ref()
                .is_some_and(|(previous, shown_at)| {
                    previous == &text && shown_at.elapsed() < Duration::from_secs(10)
                });
        if !repeated {
            state.last_mission_notification = Some((text.clone(), Instant::now()));
        }
        !repeated
    });
    if !should_show {
        return;
    }
    std::thread::spawn(move || {
        let Ok(mut agent) = start_agent_overlay_only(&gesture) else {
            return;
        };
        if !agent.capabilities.overlay {
            let _ = agent.child.kill();
            let _ = agent.child.wait();
            return;
        }
        if send_command(&mut agent.stdin, AgentCommand::OverlayShow { text }).is_err() {
            let _ = agent.child.kill();
            let _ = agent.child.wait();
            return;
        }
        let _ = send_command(
            &mut agent.stdin,
            AgentCommand::OverlayFinishResult { text: None },
        );
        // A Mission Control state notice is deliberately transient. It does
        // not take ownership of an interactive workflow or retain any task
        // transcript in the native agent.
        std::thread::sleep(Duration::from_secs(8));
        let _ = send_command(&mut agent.stdin, AgentCommand::OverlayDismiss);
        drop(agent.stdin);
        let _ = agent.child.kill();
        let _ = agent.child.wait();
    });
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

/// Requests proxied through the resident-daemon bridge (from a Home-only
/// helper process) must respect the same resumable Goal Guide state as the
/// in-process gestures, or the plan-review surface silently vanishes behind
/// a blank palette the moment Home is a separate process from the daemon.
fn resume_or_show_palette(bridge: &NativeBridge, session_id: String, prefill: Option<String>) {
    let (writer, sessions) = bridge
        .lock()
        .ok()
        .map(|state| (state.overlay_writer.clone(), state.goal_sessions.clone()))
        .unwrap_or((None, None));
    let resumed = match (&writer, &sessions) {
        (Some(writer), Some(sessions)) => match (writer.lock(), sessions.lock()) {
            (Ok(mut writer), Ok(sessions)) => {
                resume_reviewed_goal(&mut writer, &sessions).unwrap_or(false)
            }
            _ => false,
        },
        _ => false,
    };
    if !resumed
        && let Some(writer) = &writer
        && let Ok(mut writer) = writer.lock()
    {
        let _ = send_command(
            &mut writer,
            AgentCommand::PaletteShow {
                session_id,
                prefill,
            },
        );
    }
}

fn handle_native_bridge(
    mut stream: UnixStream,
    config: &mice_core::Config,
    bridge: NativeBridge,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let message: serde_json::Value = read_frame(&mut stream)?;
        match message["type"].as_str() {
            Some("daemon.ping") => {
                write_frame(&mut stream, &serde_json::json!({"type":"daemon.pong"}))?;
            }
            Some("palette.show") => {
                let prefill = message["text"]
                    .as_str()
                    .or_else(|| message["prefill"].as_str())
                    .map(|s| s.to_string());
                let session_id = message["session_id"]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("session-{}", memory::now()));
                resume_or_show_palette(&bridge, session_id, prefill);
                write_frame(&mut stream, &serde_json::json!({"type":"palette.showing"}))?;
            }
            Some("goal.show") => {
                let session_id = message["session_id"]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("session-{}", memory::now()));
                resume_or_show_palette(&bridge, session_id, Some("plan ".into()));
                write_frame(&mut stream, &serde_json::json!({"type":"goal.showing"}))?;
            }
            Some("home.show") => {
                if let Ok(state) = bridge.lock()
                    && let Some(writer) = &state.overlay_writer
                    && let Ok(mut writer) = writer.lock()
                {
                    let _ = send_command(
                        &mut writer,
                        AgentCommand::HomeShow {
                            text: home_text(config),
                        },
                    );
                }
                write_frame(&mut stream, &serde_json::json!({"type":"home.showing"}))?;
            }
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
            Some("mission.notification") => {
                if let Some(text) = message["text"].as_str() {
                    present_mission_notification(&bridge, config.gesture.clone(), text.into());
                }
            }
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
    // clickable tiles are divs the DOM snapshot cannot expose).
    let openai_vision_possible = provider_api_key("OPENAI_API_KEY").is_ok();
    let use_local_vision = config.privacy_mode == PrivacyMode::LocalOnly || !openai_vision_possible;
    let needs_screenshot = screenshot.is_none()
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

    let output = if usable_screenshot.is_some() && use_local_vision {
        call_ollama_agent_turn(
            &config.local_model,
            &goal,
            &observation,
            &history,
            usable_screenshot,
            &config.autopilot.persona,
        )?
    } else if usable_screenshot.is_some() && openai_vision_possible {
        call_openai_agent_turn(
            "gpt-5.6-sol",
            &goal,
            &observation,
            &history,
            usable_screenshot,
            &config.autopilot.persona,
        )?
    } else if config.privacy_mode == PrivacyMode::LocalOnly {
        call_ollama_agent_turn(
            &config.local_model,
            &goal,
            &observation,
            &history,
            None,
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
        && (openai_vision_possible || use_local_vision)
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
            "The legacy extension autopilot is retired. Use `mice autopilot --engine axi <goal>`; it auto-runs read-only steps and confirms mutating actions in batches."
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
    
    // Default to auto-connect for autopilot to reduce CAPTCHA triggers
    if std::env::var_os("CHROME_DEVTOOLS_AXI_AUTO_CONNECT").is_none() {
        unsafe { std::env::set_var("CHROME_DEVTOOLS_AXI_AUTO_CONNECT", "1") };
    }

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
        "Read-only actions run automatically. Mutating actions will ask for confirmation in batches. MICE never fills credentials or payment data, and never clicks sign-in, payment, transfer, or final-submission controls."
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
    // Fresh, model-decided (or repaired) actions are the expensive/risky
    // path and stay capped. Deterministic recipe replay is a separate,
    // much larger budget: it is pre-flight-verified per step and is the
    // entire point of Pillar C (many rows replaying without burning the
    // fresh-decision budget a single freehand run would need).
    let mut fresh_actions = 0_usize;
    let mut replay_actions = 0_usize;
    let mut mutating_budget = 0_usize;

    let mut goal_embedding: Option<Vec<f32>> = None;
    let mut active_recipe_steps: Vec<RecipeStep> = Vec::new();
    let mut current_sequence: Vec<RecipeStep> = Vec::new();

    while fresh_actions < AXI_FRESH_DECISION_LIMIT
        || (replay_actions < AXI_REPLAY_ACTION_LIMIT && !active_recipe_steps.is_empty())
    {
        // A stale retry belongs to this proposed action, not to the whole
        // guide. A replan does not use up a successfully-dispatched action.
        let mut recovery = AxiActionRecovery::default();
        loop {
            let observed = match observe_axi(&runner, &context) {
                Ok(observed) => observed,
                Err(error) => return pause_axi(goal, &history, &error),
            };
            
            if observed.snapshot.is_captcha() {
                println!("MICE has handed this step back to you. A CAPTCHA or security challenge was detected.");
                return Ok(());
            }

            if goal_embedding.is_none() {
                // Keyed on the goal text alone. The tab open at *start* of a
                // run is often blank/leftover and has no reliable relation
                // to the site the goal will end up on, so folding it into
                // the match key (as an earlier version of this did) made
                // retrieval and the save-time key below almost never agree.
                let embed = mice_providers::ollama_embed(&format!("{OLLAMA_ENDPOINT}/api/embed"), "nomic-embed-text", goal).unwrap_or_default();
                goal_embedding = Some(embed.clone());
                
                if !embed.is_empty() {
                    let recipes = load_recipes();
                    let mut best_sim = 0.0;
                    let mut best_recipe = None;
                    for recipe in &recipes {
                        let sim = cosine_similarity(&embed, &recipe.goal_embedding);
                        if sim > 0.85 && sim > best_sim {
                            best_sim = sim;
                            best_recipe = Some(recipe);
                        }
                    }
                    if let Some(recipe) = best_recipe {
                        println!("MICE: Found a matching recipe (score {:.2}). Replaying {} steps.", best_sim, recipe.steps.len());
                        active_recipe_steps = recipe.steps.clone();
                        active_recipe_steps.reverse();
                    }
                }
            }

            let (call, is_replay) = if let Some(recipe_step) = active_recipe_steps.pop() {
                // A recipe's uid only ever meant something in the session it
                // was recorded in (see `same_target_context`'s note on
                // backendNodeIds). Re-resolve it against the *current*
                // snapshot by (role, accessible name) before proposing it,
                // the same anchor-matching CoScripter/UiPath rely on,
                // instead of trusting a stale cross-session uid string.
                let resolved_call = match recipe_step.call.args.get("uid").and_then(Value::as_str)
                {
                    Some(_) => {
                        let resolved_uid = match (
                            recipe_step.target_role.as_deref(),
                            recipe_step.target_context.as_deref(),
                        ) {
                            (Some(role), Some(context)) => {
                                observed.snapshot.find_uid_by_identity(role, context)
                            }
                            _ => None,
                        };
                        resolved_uid.map(|uid| {
                            let mut resolved = recipe_step.call.clone();
                            if let Some(object) = resolved.args.as_object_mut() {
                                object.insert("uid".into(), Value::String(uid));
                            }
                            resolved
                        })
                    }
                    None => Some(recipe_step.call.clone()),
                };
                let Some(resolved_call) = resolved_call else {
                    println!(
                        "MICE: Recipe step '{}' no longer matches this page ({} queued recipe step(s) discarded). Falling back to guided decisions for the rest of this task.",
                        recipe_step.call.name,
                        active_recipe_steps.len()
                    );
                    active_recipe_steps.clear();
                    continue;
                };
                println!("MICE: Replaying recipe step '{}'...", resolved_call.name);
                (resolved_call, true)
            } else {
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
                        AgentAction::Done => {
                            if !current_sequence.is_empty() {
                                if let Some(embed) = goal_embedding.clone() {
                                    if !embed.is_empty() {
                                        let new_recipe = AxiRecipe {
                                            recipe_id: format!("recipe-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()),
                                            goal_pattern: goal.to_string(),
                                            goal_embedding: embed,
                                            steps: current_sequence.clone(),
                                        };
                                        if save_recipe(&new_recipe).is_ok() {
                                            println!("MICE: Saved successful sequence as a new recipe.");
                                        }
                                    }
                                }
                            }
                            println!(
                                "MICE AXI guide complete: {}",
                                decision
                                    .done_summary
                                    .as_deref()
                                    .unwrap_or("The requested step is complete.")
                            );
                        },
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
                (call, false)
            };

            let tool_kind = tools::specs().iter().find(|s| s.name == call.name).map(|s| s.kind).unwrap_or(tools::ToolKind::Mutating);

            if is_replay {
                println!(
                    "Proposed replay action ({} queued after this): {}",
                    active_recipe_steps.len(),
                    observed.snapshot.approval_summary(&call)
                );
            } else {
                println!(
                    "Proposed action {}/{}: {}",
                    fresh_actions + 1,
                    AXI_FRESH_DECISION_LIMIT,
                    observed.snapshot.approval_summary(&call)
                );
            }

            if tool_kind == tools::ToolKind::Mutating {
                if mutating_budget == 0 {
                    let batch_size = config.autopilot.checkpoint_batch_size;
                    if batch_size <= 1 {
                        print!("Do this one action? [y/N] ");
                    } else {
                        print!("Allow this and up to {} more mutating actions automatically? [y/N] ", batch_size.saturating_sub(1));
                    }
                    std::io::stdout().flush()?;
                    let mut consent = String::new();
                    std::io::stdin().read_line(&mut consent)?;
                    if !matches!(consent.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        println!("MICE did not act. You can continue manually or run a narrower goal.");
                        return Ok(());
                    }
                    mutating_budget = batch_size;
                }
                // Budget is only spent once the action below actually
                // succeeds (see the `continue` sites): a replan or a stale
                // retry must not silently shrink an already-approved batch.
            } else {
                println!("(Auto-approving read-only action)");
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

            if tool_kind == tools::ToolKind::Mutating {
                mutating_budget = mutating_budget.saturating_sub(1);
            }
            if is_replay {
                replay_actions += 1;
            } else {
                fresh_actions += 1;
            }
            // Record the step that actually ran, anchored by the identity
            // it resolved to just now, so a future recipe replay can
            // re-resolve it against a different page instead of reusing
            // this run's raw (session-scoped) uid.
            let identity = call
                .args
                .get("uid")
                .and_then(Value::as_str)
                .and_then(|uid| current.snapshot.identity_of(uid));
            current_sequence.push(RecipeStep {
                call: call.clone(),
                target_role: identity.as_ref().map(|(role, _)| role.clone()),
                target_context: identity.as_ref().map(|(_, context)| context.clone()),
            });
            break;
        }
    }
    Err(
        "MICE AXI guide reached its fresh-decision limit with no matching recipe to replay. Continue with a narrower follow-up goal."
            .into(),
    )
}

const AXI_FRESH_DECISION_LIMIT: usize = 6;
// Generous but bounded: a recorded recipe can have at most as many steps as
// the fresh-decision run that taught it, so this ceiling only matters as a
// safety valve against a corrupted or hand-edited recipe file.
const AXI_REPLAY_ACTION_LIMIT: usize = 200;

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
        
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "say_to_user": { "type": "string" },
                "action": { "type": "string", "enum": ["click", "fill", "open_url", "scroll", "done", "handoff", "ask_user"] },
                "candidate_id": { "type": ["string", "null"] },
                "url": { "type": ["string", "null"] },
                "value": { "type": ["string", "null"] },
                "done_summary": { "type": ["string", "null"] },
                "question": { "type": ["string", "null"] }
            },
            "required": ["say_to_user", "action"],
            "additionalProperties": false
        });

        stream_ollama_with_format(
            &config.tool_model,
            instruction,
            Some(&format!(
                "Goal: {goal}\n\nCurrent AXI snapshot:\n{observation}\n\nPrior actions:\n{history}"
            )),
            Some(schema),
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
    stop_managed_ollama();
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
    ureq::AgentBuilder::new()
        // A generation is streamed by the caller. Bound connection setup but
        // do not turn this into a 45-second whole-response deadline: slow,
        // healthy Ollama/OpenAI/Groq streams must be allowed to finish.
        .timeout_connect(Duration::from_secs(10))
        .build()
        .post(endpoint)
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
    let api_key = provider_api_key("OPENAI_API_KEY")?;
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
    let api_key = provider_api_key("GROQ_API_KEY")?;
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
    let api_key = provider_api_key("OPENAI_API_KEY")?;
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
    let api_key = provider_api_key("GROQ_API_KEY")?;
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

/// Mission Control is deliberately local-first even when normal UI requests
/// may use a cloud lane. A repository plan and its path index stay on-device
/// unless the developer explicitly passes `mice mission plan --allow-cloud`.
pub(crate) fn mission_planner_response(
    prompt: &str,
    allow_cloud: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let configuration = config()?;
    let local_error = if configuration.privacy_mode != PrivacyMode::CloudOnly {
        let mut response = String::new();
        match stream_ollama(
            &configuration.local_model,
            "Return a validated MICE Mission Control task graph as specified in the content. JSON only.",
            Some(prompt),
            |chunk| {
                response.push_str(chunk);
                Ok(())
            },
        ) {
            Ok(()) if !response.trim().is_empty() => return Ok(response),
            Ok(()) => "local planner returned no output".into(),
            Err(error) => error.to_string(),
        }
    } else {
        "privacy mode is cloud_only".into()
    };
    if !allow_cloud {
        return Err(format!(
            "local mission planner unavailable ({}) and cloud planning was not explicitly allowed",
            local_error
        )
        .into());
    }
    if configuration.privacy_mode == PrivacyMode::LocalOnly {
        return Err(
            "local mission planner failed and local-only mode forbids cloud planning".into(),
        );
    }
    if is_groq_model(&configuration.cloud_model) {
        call_groq_mission_plan(&configuration.cloud_model, prompt)
    } else {
        call_openai_mission_plan(&configuration.cloud_model, prompt)
    }
}

fn call_openai_mission_plan(
    model: &str,
    prompt: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = provider_api_key("OPENAI_API_KEY")?;
    let payload = mice_providers::structured_mission_plan_payload(model, prompt).to_string();
    let response = post_provider_json(
        "OpenAI Mission Control planner",
        "https://api.openai.com/v1/responses",
        &api_key,
        &payload,
    )?;
    response["output"]
        .as_array()
        .and_then(|items| items.iter().find_map(|item| item["content"].as_array()))
        .and_then(|content| content.iter().find_map(|part| part["text"].as_str()))
        .map(str::to_owned)
        .ok_or_else(|| "OpenAI Mission Control planner returned no structured graph".into())
}

fn call_groq_mission_plan(model: &str, prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = provider_api_key("GROQ_API_KEY")?;
    let payload = mice_providers::groq_mission_plan_payload(model, prompt).to_string();
    let response = post_provider_json(
        "Groq Mission Control planner",
        "https://api.groq.com/openai/v1/chat/completions",
        &api_key,
        &payload,
    )?;
    response["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .and_then(|choice| choice["message"]["content"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "Groq Mission Control planner returned no JSON graph".into())
}

fn call_ollama_agent_turn(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    image_data_url: Option<&str>,
    persona: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let payload = mice_providers::ollama_agent_loop_payload_with_persona(
        model,
        goal,
        observation,
        history,
        image_data_url,
        persona,
    )
    .to_string();
    let response = ureq::post("http://127.0.0.1:11434/api/chat")
        .set("Content-Type", "application/json")
        .send_string(&payload)?;
    let body: serde_json::Value = serde_json::from_reader(response.into_reader())?;
    body["message"]["content"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "Ollama autopilot response did not contain text content.".into())
}

fn call_openai_agent_turn(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    image_data_url: Option<&str>,
    persona: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let api_key = provider_api_key("OPENAI_API_KEY")?;
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
    let api_key = provider_api_key("GROQ_API_KEY")?;
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

const MICE_KEYCHAIN_SERVICE: &str = "MICE";
const KEYCHAIN_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Cloud keys belong in the user's login Keychain, never in MICE's TOML
/// config, history, repository, or a long-lived shell environment. Environment
/// variables still take precedence for CI and one-off developer overrides.
fn provider_api_key(variable: &str) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(value) = env::var(variable)
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    keychain_api_key(variable).ok_or_else(|| {
        format!(
            "{variable} is not configured. Run `mice keys set {}` to save it securely in your macOS Keychain.",
            provider_key_name(variable).unwrap_or("provider")
        )
        .into()
    })
}

fn provider_key_name(variable: &str) -> Option<&'static str> {
    match variable {
        "GROQ_API_KEY" => Some("groq"),
        "OPENAI_API_KEY" => Some("openai"),
        _ => None,
    }
}

fn provider_key_variable(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "groq" => Some("GROQ_API_KEY"),
        "openai" | "open_ai" => Some("OPENAI_API_KEY"),
        _ => None,
    }
}

fn keychain_api_key(variable: &str) -> Option<String> {
    provider_key_name(variable)?;
    let mut child = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            MICE_KEYCHAIN_SERVICE,
            "-a",
            variable,
            "-w",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + KEYCHAIN_LOOKUP_TIMEOUT;
    while Instant::now() < deadline {
        if child.try_wait().ok()?.is_some() {
            let output = child.wait_with_output().ok()?;
            if !output.status.success() {
                return None;
            }
            let value = String::from_utf8(output.stdout).ok()?;
            let value = value.trim_end_matches(['\r', '\n']).to_owned();
            return (!value.is_empty()).then_some(value);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn keys() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("status") if arguments.len() == 1 => {
            for (label, variable) in [("Groq", "GROQ_API_KEY"), ("OpenAI", "OPENAI_API_KEY")] {
                let source = if env::var_os(variable).is_some() {
                    "environment override"
                } else if keychain_api_key(variable).is_some() {
                    "macOS Keychain"
                } else {
                    "not configured"
                };
                println!("{label}: {source}");
            }
            Ok(())
        }
        Some("set") if arguments.len() == 2 => {
            let name = provider_key_variable(&arguments[1])
                .ok_or("Choose `groq` or `openai`.")?;
            let secret = read_keychain_secret(&arguments[1])?;
            save_keychain_api_key(name, &secret)?;
            println!("Saved {} securely in your macOS Keychain.", arguments[1].to_ascii_uppercase());
            println!("Restart MICE (`mice stop`, then `mice`) if it is already running.");
            Ok(())
        }
        Some("delete") | Some("remove") if arguments.len() == 2 => {
            let variable = provider_key_variable(&arguments[1])
                .ok_or("Choose `groq` or `openai`.")?;
            confirm_change(
                &format!("Delete MICE's {} key from your macOS Keychain?", arguments[1]),
                false,
            )?;
            let status = Command::new("/usr/bin/security")
                .args([
                    "delete-generic-password",
                    "-s",
                    MICE_KEYCHAIN_SERVICE,
                    "-a",
                    variable,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if !status.success() {
                return Err("No matching MICE key was found in your macOS Keychain.".into());
            }
            println!("Deleted MICE's {} key from your macOS Keychain.", arguments[1]);
            Ok(())
        }
        _ => Err("Usage: mice keys <set|status|delete> <groq|openai> (use `mice keys status` without a provider)".into()),
    }
}

/// Read a key with normal terminal line input. Some terminals encode a paste
/// in raw mode with bracketed-paste markers (`\x1b[200~` / `\x1b[201~`), which
/// would silently corrupt the credential. Visible input is the reliable
/// temporary UX; the value still never enters shell history or a command
/// argument and is handed to Keychain over stdin.
fn read_keychain_secret(provider: &str) -> Result<String, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() {
        return Err("`mice keys set` requires an interactive terminal.".into());
    }
    print!("Paste your {provider} API key (visible): ");
    std::io::stdout().flush()?;
    let mut secret = String::new();
    std::io::stdin().read_line(&mut secret)?;
    // Defensive cleanup for a pasted value copied from a terminal that has
    // already leaked its bracketed-paste envelope into the line buffer.
    let secret = secret
        .trim()
        .trim_start_matches("\u{1b}[200~")
        .trim_start_matches("[200~")
        .trim_end_matches("\u{1b}[201~")
        .trim_end_matches("[201~")
        .trim()
        .to_owned();
    if secret.is_empty() {
        Err("The API key was empty; nothing was saved.".into())
    } else {
        Ok(secret)
    }
}

fn save_keychain_api_key(variable: &str, secret: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = Command::new("/usr/bin/security")
        // Per `security help add-generic-password`, a final `-w` with no
        // value reads the password from stdin. Do not replace this with
        // `-w <secret>`: process arguments are observable by other apps.
        .args([
            "add-generic-password",
            "-a",
            variable,
            "-s",
            MICE_KEYCHAIN_SERVICE,
            "-U",
            "-w",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("Keychain input was unavailable")?;
    // `security add-generic-password -w` reads the password twice to
    // confirm it. It can exit successfully even when the confirmation is
    // missing, leaving an unusable empty keychain entry, so provide both
    // interactive answers through the private stdin pipe.
    stdin.write_all(secret.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.write_all(secret.as_bytes())?;
    stdin.write_all(b"\n")?;
    drop(stdin);
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() && !stderr.contains("passwords don't match") {
        Ok(())
    } else {
        Err(
            "macOS Keychain could not save the key. Unlock your login keychain and try again."
                .into(),
        )
    }
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

fn user_history() -> Result<memory::UserHistory, Box<dyn std::error::Error>> {
    let path = memory::UserHistory::default_path().ok_or("HOME is not set")?;
    Ok(memory::UserHistory::at(path)?)
}

fn record_user_history(
    kind: memory::HistoryKind,
    question: &str,
    answer: &str,
    app_context: Option<String>,
) {
    if let Ok(history) = user_history() {
        let _ = history.record(memory::HistoryEvent {
            ts: memory::now(),
            kind,
            question: question.into(),
            answer_digest: answer.into(),
            app_context,
        });
    }
}

/// Captured screen/selection source material must never be reconstructed from
/// history. Record only an event marker, not the model's answer, because an
/// answer can quote the private source verbatim.
fn record_sensitive_history(
    kind: memory::HistoryKind,
    question: &str,
    app_context: Option<String>,
) {
    record_user_history(
        kind,
        question,
        "Completed. Source-derived response was intentionally not retained.",
        app_context,
    );
}

fn history() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(2).collect::<Vec<_>>();
    let store = user_history()?;
    if arguments
        .first()
        .is_some_and(|argument| argument == "--clear")
    {
        if arguments.len() != 1 {
            return Err("Usage: mice history --clear".into());
        }
        store.clear()?;
        println!("MICE history and preferences cleared.");
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--remember")
    {
        store.remember(&arguments.get(1..).unwrap_or_default().join(" "))?;
        println!("MICE will apply that local preference to future answers.");
        return Ok(());
    }
    let query = (!arguments.is_empty()).then(|| arguments.join(" "));
    let entries = store.search(query.as_deref())?;
    if entries.is_empty() {
        println!("No matching MICE history.");
        return Ok(());
    }
    for entry in entries.into_iter().take(50) {
        let app = entry
            .app_context
            .as_deref()
            .map(|value| format!(" — {value}"))
            .unwrap_or_default();
        println!(
            "- [{}] {}: {}{}",
            entry.ts,
            format!("{:?}", entry.kind).to_lowercase(),
            entry.question,
            app
        );
        println!("  {}", entry.answer_digest);
    }
    Ok(())
}

/// Show the plans a person explicitly asked MICE to remember. Plans are kept
/// in the owner-only history store and are never reconstructed from screen,
/// clipboard, or selection content.
fn plans() -> Result<(), Box<dyn std::error::Error>> {
    let plans = user_history()?
        .search(None)?
        .into_iter()
        .filter(|entry| entry.kind == memory::HistoryKind::GoalPlan)
        .collect::<Vec<_>>();
    if plans.is_empty() {
        println!("No saved plans yet. Use Ctrl+Option+Space or type `plan <goal>` in MICE.");
        return Ok(());
    }
    for (index, plan) in plans.into_iter().take(20).enumerate() {
        println!("{}. {}", index + 1, plan.question);
        println!("{}", plan.answer_digest);
        println!();
    }
    Ok(())
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
        "You are paired with MICE, a local execution manager. Delegate mechanical, repetitive, or token-heavy steps through run_tool or delegate_task; MICE first uses deterministic local CLIs and returns bounded results. For an assigned repository mission, call mission_status before editing; check team_status before editing shared files and record durable choices with memory_note.\n\nAvailable deterministic tools:\n{}",
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
    ensure_local_model(config)?;
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
    let Some(start) = value.find('{') else {
        return value;
    };
    let Some(relative_end) = value[start..].rfind('}') else {
        return value;
    };
    &value[start..start + relative_end + 1]
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
        if provider_api_key("OPENAI_API_KEY").is_ok() {
            "set"
        } else {
            "not set"
        }
    );
    println!(
        "GROQ_API_KEY: {}",
        if provider_api_key("GROQ_API_KEY").is_ok() {
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
    println!("  Autopilot mode defaults to CHROME_DEVTOOLS_AXI_AUTO_CONNECT=1 to attach to a running Chrome profile and reduce CAPTCHA triggers.");
    Ok(())
}

fn settings() -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path().ok_or("HOME is not set")?;
    let mut config = load_config(&path)?;
    if run_settings_tui(&mut config)? {
        // Validate a prospective Local Only setting before persisting it. A
        // failed disk/Ollama/model check must leave the known-good config in
        // place for the resident daemon and the next launch.
        ensure_local_only_ready(&config)?;
        save_config(&path, &config)?;
        println!("Saved settings to {}.", path.display());
        // The resident daemon owns an immutable routing snapshot. Restart it
        // after an explicit settings save so a privacy-mode switch takes
        // effect immediately rather than silently waiting for a later launch.
        if UnixStream::connect(bridge_socket_path()?).is_ok() {
            stop()?;
            launch()?;
            println!("Restarted MICE with the new settings.");
        }
    }
    Ok(())
}

const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";
const DEFAULT_AUTOMATIC_LOCAL_MODEL: &str = "gemma3:4b";
const MIN_AUTOMATIC_MODEL_DISK_GIB: u64 = 8;

fn ollama_models_path() -> PathBuf {
    env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".ollama/models")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn available_disk_gib() -> u64 {
    let mut volume_path = ollama_models_path();
    while !volume_path.exists() {
        let Some(parent) = volume_path.parent() else {
            break;
        };
        volume_path = parent.to_owned();
    }
    Command::new("df")
        .arg("-g")
        .arg(volume_path)
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| {
            output
                .lines()
                .nth(1)
                .and_then(|line| line.split_whitespace().nth(3))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

fn ollama_available() -> bool {
    Command::new("ollama")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn managed_ollama_pid_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    config_path()
        .and_then(|path| {
            path.parent()
                .map(|parent| parent.join("managed-ollama.pid"))
        })
        .ok_or_else(|| "HOME is not set".into())
}

fn process_start_marker(pid: u32) -> Option<String> {
    Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|marker| !marker.is_empty())
}

fn remember_managed_ollama(pid: u32) {
    if let (Ok(path), Some(marker)) = (managed_ollama_pid_path(), process_start_marker(pid)) {
        let _ = std::fs::write(path, format!("{pid}\t{marker}"));
    }
}

fn stop_managed_ollama() {
    let Ok(path) = managed_ollama_pid_path() else {
        return;
    };
    let record = std::fs::read_to_string(&path).ok();
    let parsed = record.as_deref().and_then(|value| {
        let (pid, marker) = value.trim().split_once('\t')?;
        Some((pid.parse::<u32>().ok()?, marker.to_owned()))
    });
    let Some((pid, expected_marker)) = parsed else {
        let _ = std::fs::remove_file(path);
        return;
    };
    let command = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).to_ascii_lowercase())
        .unwrap_or_default();
    if command.contains("ollama")
        && command.contains("serve")
        && process_start_marker(pid).as_deref() == Some(expected_marker.as_str())
    {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(path);
}

/// Start an Ollama server only when no one else is already serving it. Ollama
/// is intentionally launched without a shell, keeping user configuration and
/// provider keys out of the child environment contract.
fn ensure_ollama_server() -> Result<(), Box<dyn std::error::Error>> {
    if mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, DEFAULT_AUTOMATIC_LOCAL_MODEL).is_ok()
        || ureq::get(&format!("{OLLAMA_ENDPOINT}/api/tags"))
            .timeout(Duration::from_secs(1))
            .call()
            .is_ok()
    {
        return Ok(());
    }
    if !ollama_available() {
        return Err("Ollama is not installed. Install it from https://ollama.com, then run `mice setup` again.".into());
    }
    let child = Command::new("ollama")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    remember_managed_ollama(child.id());
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{OLLAMA_ENDPOINT}/api/tags"))
            .timeout(Duration::from_secs(1))
            .call()
            .is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("Ollama did not become ready within 20 seconds. Run `ollama serve` for diagnostics.".into())
}

/// Local Only is the one mode where MICE must make the local lane ready before
/// accepting work. The automatic download boundary is deliberately narrow:
/// only the default small model is pulled; heavier or user-selected alternates
/// remain an explicit choice in settings.
fn ensure_local_model(config: &mice_core::Config) -> Result<(), Box<dyn std::error::Error>> {
    ensure_ollama_server()?;
    if mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, &config.local_model).is_ok() {
        return Ok(());
    }
    if config.local_model != DEFAULT_AUTOMATIC_LOCAL_MODEL {
        return Err(format!(
            "The selected local model `{}` is not installed. MICE only auto-downloads `{DEFAULT_AUTOMATIC_LOCAL_MODEL}`; select it in `mice settings` or run `ollama pull {}` yourself.",
            config.local_model, config.local_model
        )
        .into());
    }
    let available = available_disk_gib();
    if available < MIN_AUTOMATIC_MODEL_DISK_GIB {
        return Err(format!(
            "MICE needs at least {MIN_AUTOMATIC_MODEL_DISK_GIB} GiB free to download `{DEFAULT_AUTOMATIC_LOCAL_MODEL}` (only {available} GiB available)."
        )
        .into());
    }
    println!("Preparing private mode: downloading `{DEFAULT_AUTOMATIC_LOCAL_MODEL}` once…");
    let status = Command::new("ollama")
        .args(["pull", DEFAULT_AUTOMATIC_LOCAL_MODEL])
        .status()?;
    if !status.success() {
        return Err(format!("Ollama could not download `{DEFAULT_AUTOMATIC_LOCAL_MODEL}`.").into());
    }
    mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, &config.local_model).map_err(|error| {
        format!(
            "Ollama finished but `{}` is still unavailable: {error}",
            config.local_model
        )
        .into()
    })
}

fn ensure_local_only_ready(config: &mice_core::Config) -> Result<(), Box<dyn std::error::Error>> {
    if config.privacy_mode == PrivacyMode::LocalOnly {
        ensure_local_model(config)?;
    }
    Ok(())
}

fn run_settings_tui(config: &mut mice_core::Config) -> Result<bool, Box<dyn std::error::Error>> {
    // Keychain/Ollama checks happen once on entry, never inside the 250 ms
    // render loop. Settings must remain responsive even if Keychain prompts
    // or Ollama is unavailable.
    let availability = SettingsAvailability {
        local_model_ready: mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, &config.local_model)
            .is_ok(),
        groq_key_available: provider_api_key("GROQ_API_KEY").is_ok(),
        openai_key_available: provider_api_key("OPENAI_API_KEY").is_ok(),
    };
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = settings_event_loop(&mut terminal, config, availability);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn settings_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    config: &mut mice_core::Config,
    availability: SettingsAvailability,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut selected = 0usize;
    loop {
        terminal.draw(|frame| draw_settings(frame, config, selected, availability))?;
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
            KeyCode::Up | KeyCode::Char('k') => selected = selected.checked_sub(1).unwrap_or(11),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % 12,
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

#[derive(Clone, Copy)]
struct SettingsAvailability {
    local_model_ready: bool,
    groq_key_available: bool,
    openai_key_available: bool,
}

fn draw_settings(
    frame: &mut ratatui::Frame,
    config: &mice_core::Config,
    selected: usize,
    availability: SettingsAvailability,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(8)])
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
        format!("Command palette   {}", config.gesture.palette_trigger),
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
    let overview = settings_routing_overview(config, availability);
    frame.render_widget(
        Paragraph::new(overview).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Active routing "),
        ),
        chunks[1],
    );
}

fn settings_routing_overview(
    config: &mice_core::Config,
    availability: SettingsAvailability,
) -> String {
    let local_status = if availability.local_model_ready {
        "ready"
    } else {
        "not ready"
    };
    let cloud_provider = if is_groq_model(&config.cloud_model) {
        "Groq"
    } else {
        "OpenAI"
    };
    let cloud_key_ready = match cloud_provider {
        "Groq" => availability.groq_key_available,
        _ => availability.openai_key_available,
    };
    let cloud_status = if cloud_key_ready {
        "key available"
    } else {
        "KEY MISSING"
    };
    let routing = match config.privacy_mode {
        PrivacyMode::LocalOnly => format!(
            "All supported text and Goal Guide work → LOCAL {} ({local_status}). Cloud is blocked.",
            config.local_model
        ),
        PrivacyMode::CloudOnly => format!(
            "All supported work → CLOUD {} via {cloud_provider} ({cloud_status}).",
            config.cloud_model
        ),
        PrivacyMode::CloudAllowed => format!(
            "Text / hover / selection summaries → LOCAL {} ({local_status}). Goal Guide / browser / images → CLOUD {} via {cloud_provider} ({cloud_status}).",
            config.local_model, config.cloud_model
        ),
    };
    format!(
        "{routing}\n\nGroq key: {}    OpenAI key: {}\nUse `mice keys set groq` or `mice keys set openai` to save a key.\n↑/↓ select  ←/→ change  s save  q cancel",
        if availability.groq_key_available {
            "available"
        } else {
            "not configured"
        },
        if availability.openai_key_available {
            "available"
        } else {
            "not configured"
        },
    )
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
            &mut config.gesture.palette_trigger,
            &["ctrl+shift+space", "ctrl+alt+space", "cmd+shift+space"],
            forward,
        ),
        10 => cycle_value(
            &mut config.autopilot.persona,
            &["patient", "concise", "playful"],
            forward,
        ),
        11 => config.autopilot.careful_mode = !config.autopilot.careful_mode,
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
    ensure_local_only_ready(&config)?;
    let browser_goal_directive: NativeBridge = Arc::new(Mutex::new(NativeBridgeState::default()));
    // Readiness is only advertised after the native agent has started. The
    // bridge socket is deliberately bound second so it cannot be a false
    // positive when the agent binary or its permissions fail immediately.
    let mut agent = start_agent_daemon(&config.gesture)?;
    let agent_writer = Arc::new(Mutex::new(agent.stdin));
    if let Ok(mut state) = browser_goal_directive.lock() {
        state.overlay_writer = Some(Arc::clone(&agent_writer));
    }
    if let Err(error) = start_native_bridge(config.clone(), Arc::clone(&browser_goal_directive)) {
        let _ = agent.child.kill();
        let _ = agent.child.wait();
        return Err(error);
    }
    let goal_sessions = Arc::new(Mutex::new(HashMap::<String, GoalSession>::new()));
    let goal_plans = Arc::new(Mutex::new(HashMap::<String, GoalPlanResult>::new()));
    let active_guides = Arc::new(Mutex::new(HashMap::<String, ActiveGuide>::new()));
    if let Ok(mut state) = browser_goal_directive.lock() {
        state.goal_sessions = Some(Arc::clone(&goal_sessions));
    }
    // Best-effort warm path: daemon startup never waits for Ollama and never
    // fails in cloud-only mode, but local requests avoid a cold model load.
    if config.privacy_mode != PrivacyMode::CloudOnly {
        let warm_model = config.local_model.clone();
        std::thread::spawn(move || {
            let _ =
                mice_providers::warm_ollama_model("http://127.0.0.1:11434/api/chat", &warm_model);
        });
    }
    let watchdog_bridge = Arc::clone(&browser_goal_directive);
    std::thread::spawn(move || {
        loop {
            recover_autopilot_timeouts(&watchdog_bridge);
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    std::thread::spawn(move || {
        loop {
            if let Ok(store) = schedule_store() {
                let now = memory::now();
                if let Ok(due) = store.due_tasks(now) {
                    for task in due {
                        let _ = store.mark_triggered(&task.id);
                        match task.action {
                            ScheduleAction::Reminder { message } => {
                                eprintln!("[MICE Schedule] Reminder: {message}");
                            }
                            ScheduleAction::ExecuteGoal { goal, .. } => {
                                eprintln!("[MICE Schedule] Goal due: {goal}");
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    println!(
        "MICE is running with {} agent (overlay={}). Press Ctrl-C to stop.",
        agent.platform, agent.capabilities.overlay
    );
    println!("=== MICE Keyboard Gesture Loop ===");
    println!(
        "Capture: Ctrl+Shift+Space. Hover: hold Control. Select text, then Ctrl double-tap to summarize or Ctrl+Option+I for an infographic. After a normal Cmd-C, Ctrl+Option+C smart-copies."
    );
    if env::var_os("MICE_OPEN_HOME").is_some() {
        send_command(
            &mut agent_writer.lock().unwrap(),
            AgentCommand::HomeShow {
                text: home_text(&config),
            },
        )?;
    }

    let mut selection_cache: Option<SelectionCache> = None;
    // At most one speculative deeper explanation runs at once. This keeps a
    // local model responsive if someone makes several selections in a row.
    let go_deeper_prefetch_in_flight = Arc::new(AtomicBool::new(false));
    while let Ok(msg) = read_frame::<mice_ipc::RpcNotification>(&mut agent.reader) {
        if msg.method == "goal.request" {
            // Goal Guide is resumable. Losing focus or pressing Escape only
            // hides the native surface; the reviewed plan remains in core
            // state until the person starts, revises, or cancels it.
            if resume_reviewed_goal(
                &mut agent_writer.lock().unwrap(),
                &goal_sessions.lock().unwrap(),
            )? {
                continue;
            }
            let session_id = msg.params["sessionId"]
                .as_str()
                .ok_or("MICE received a goal request without a session ID")?;
            send_command(
                &mut agent_writer.lock().unwrap(),
                AgentCommand::PaletteShow {
                    session_id: session_id.into(),
                    prefill: Some("plan ".into()),
                },
            )?;
        } else if msg.method == "palette.request" {
            // The everyday Ask MICE gesture is another entry point into the
            // same resumable Goal Guide state as "goal.request". A blank
            // local palette must never silently stand in for an active plan
            // that is still planning or awaiting review.
            if resume_reviewed_goal(
                &mut agent_writer.lock().unwrap(),
                &goal_sessions.lock().unwrap(),
            )? {
                continue;
            }
            let session_id = msg.params["sessionId"]
                .as_str()
                .ok_or("MICE received a palette request without a session ID")?;
            let prefill = msg.params["prefill"].as_str().map(|s| s.to_string());
            send_command(
                &mut agent_writer.lock().unwrap(),
                AgentCommand::PaletteShow {
                    session_id: session_id.into(),
                    prefill,
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
            if let Err(error) = handle_goal_submission(
                &agent_writer,
                &config,
                &goal_sessions,
                &goal_plans,
                &active_guides,
                &browser_goal_directive,
                submission,
            ) {
                eprintln!("MICE goal planning failed: {error}");
            }
        } else if msg.method == "prompt.cancelled" {
            if let Some(session_id) = msg.params["sessionId"].as_str() {
                if let Ok(mut sessions) = goal_sessions.lock() {
                    sessions.remove(session_id);
                }
                if let Ok(mut plans) = goal_plans.lock() {
                    plans.remove(session_id);
                }
                if let Ok(mut guides) = active_guides.lock() {
                    guides.remove(session_id);
                }
                clear_browser_goal_directive(&browser_goal_directive);
            }
            let _ = send_command(
                &mut agent_writer.lock().unwrap(),
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
            let mut guides_guard = active_guides.lock().unwrap();
            match handle_guide_control(
                &mut agent_writer.lock().unwrap(),
                &mut guides_guard,
                &browser_goal_directive,
                session_id,
                action,
                value,
            ) {
                Ok(true) => {
                    if let Ok(mut sessions) = goal_sessions.lock() {
                        sessions.remove(session_id);
                    }
                    if let Ok(mut plans) = goal_plans.lock() {
                        plans.remove(session_id);
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!("MICE guide control failed: {error}"),
            }
        } else if msg.method == "palette.dismissed" {
            let dismissed: mice_ipc::PaletteDismissed = match serde_json::from_value(msg.params) {
                Ok(dismissed) => dismissed,
                Err(error) => {
                    eprintln!("MICE received an invalid palette dismissal: {error}");
                    continue;
                }
            };
            // Pressing Escape hides the native palette/overlay panel, but does
            // NOT cancel or erase an active goal plan session. Retain sessions
            // that are currently planning or ready for review.
            if let Ok(mut sessions) = goal_sessions.lock() {
                let is_active_plan = sessions.get(&dismissed.session_id).is_some_and(|s| {
                    matches!(
                        s.state(),
                        GoalState::Planning { .. } | GoalState::Reviewing { .. }
                    )
                });
                if !is_active_plan {
                    sessions.remove(&dismissed.session_id);
                    if let Ok(mut plans) = goal_plans.lock() {
                        plans.remove(&dismissed.session_id);
                    }
                    if let Ok(mut guides) = active_guides.lock() {
                        guides.remove(&dismissed.session_id);
                    }
                    clear_browser_goal_directive(&browser_goal_directive);
                }
            }
        } else if msg.method == "palette.submitted" {
            let submission: PaletteSubmitted = match serde_json::from_value(msg.params) {
                Ok(submission) => submission,
                Err(error) => {
                    eprintln!("MICE received invalid palette input: {error}");
                    continue;
                }
            };
            let palette_session = submission.session_id.clone();
            if let Err(error) = handle_palette_submission(
                &agent_writer,
                &config,
                &goal_sessions,
                &goal_plans,
                &active_guides,
                &browser_goal_directive,
                submission,
            ) {
                eprintln!("MICE palette request failed: {error}");
                if let Ok(mut sessions) = goal_sessions.lock() {
                    sessions.remove(&palette_session);
                }
                if let Ok(mut plans) = goal_plans.lock() {
                    plans.remove(&palette_session);
                }
                if let Ok(mut guides) = active_guides.lock() {
                    guides.remove(&palette_session);
                }
                clear_browser_goal_directive(&browser_goal_directive);
                let _ = send_command(
                    &mut agent_writer.lock().unwrap(),
                    AgentCommand::PaletteFinishResult {
                        session_id: palette_session,
                        text: Some(format!("MICE could not complete that: {error}")),
                    },
                );
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
            match handle_selection_action(&mut agent_writer.lock().unwrap(), &config, selection) {
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
            if let Err(error) =
                handle_smart_copy(&mut agent_writer.lock().unwrap(), &config, captured)
            {
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
            let goal_action = matches!(
                action_id.as_str(),
                "goal.accept" | "goal.revise" | "goal.cancel"
            );
            let result = if goal_action {
                handle_goal_review_action(
                    &mut agent_writer.lock().unwrap(),
                    &mut goal_sessions.lock().unwrap(),
                    &mut goal_plans.lock().unwrap(),
                    &mut active_guides.lock().unwrap(),
                    &browser_goal_directive,
                    &session_id,
                    &action_id,
                )
            } else {
                handle_overlay_action(
                    &mut agent_writer.lock().unwrap(),
                    &config,
                    &mut selection_cache,
                    &session_id,
                    &action_id,
                )
            };
            if let Err(error) = result {
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
                &mut agent_writer.lock().unwrap(),
                AgentCommand::OverlayShow {
                    text: "MICE is thinking…".into(),
                },
            )?;

            if action == Action::Image {
                match generate_and_present_image(
                    &mut agent_writer.lock().unwrap(),
                    &request.instruction,
                ) {
                    Ok(()) => {
                        println!("[gpt-image-2] Infographic generated and copied to the clipboard.")
                    }
                    Err(error) => {
                        println!("Image generation error: {error}");
                        let _ = send_command(
                            &mut agent_writer.lock().unwrap(),
                            AgentCommand::OverlayFinishResult {
                                text: Some(format!("Image generation error: {error}")),
                            },
                        );
                    }
                }
                continue;
            }

            let mut stdin_guard = agent_writer.lock().unwrap();
            let mut stream = OverlayStream::echoing(&mut stdin_guard);
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
                        send_command(
                            &mut agent_writer.lock().unwrap(),
                            clipboard_command(&response),
                        )?;
                    }
                    send_command(
                        &mut agent_writer.lock().unwrap(),
                        AgentCommand::OverlayFinishResult { text: None },
                    )?;
                }
                Err(error) => {
                    let _ = stream.finish();
                    println!("Error: {}", error);
                    let _ = send_command(
                        &mut agent_writer.lock().unwrap(),
                        AgentCommand::OverlayFinishResult {
                            text: Some(format!("Error: {error}")),
                        },
                    );
                }
            }
        } else if msg.method == "hover.captured" {
            // A reviewed plan owns the visible guidance surface. Hover events
            // captured just before the palette was submitted must not replace
            // its Planning/Review panel after the model returns.
            if goal_sessions.lock().unwrap().values().any(|session| {
                matches!(
                    session.state(),
                    GoalState::Planning { .. }
                        | GoalState::Reviewing { .. }
                        | GoalState::Accepted { .. }
                )
            }) {
                continue;
            }
            let hover: HoverCaptured = match serde_json::from_value(msg.params) {
                Ok(hover) => hover,
                Err(error) => {
                    eprintln!("MICE received invalid hover context: {error}");
                    continue;
                }
            };
            if let Err(error) = explain_hover(&mut agent_writer.lock().unwrap(), &config, hover) {
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
        SelectionAction::Define => Action::Define,
        SelectionAction::Image => Action::Image,
    };
    let instruction = if action == Action::Summarize {
        selection_summary_instruction(&selection.text).into()
    } else {
        action_instruction(action, "")
    };
    let instruction = apply_preferences(
        &instruction,
        user_history()
            .ok()
            .and_then(|history| history.preferences_preamble().ok().flatten())
            .as_deref(),
    );
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
                record_sensitive_history(
                    memory::HistoryKind::Summarize,
                    "A selected-text summary",
                    None,
                );
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
    agent_writer: &Arc<Mutex<ChildStdin>>,
    config: &mice_core::Config,
    goal_sessions: &Arc<Mutex<HashMap<String, GoalSession>>>,
    goal_plans: &Arc<Mutex<HashMap<String, GoalPlanResult>>>,
    active_guides: &Arc<Mutex<HashMap<String, ActiveGuide>>>,
    browser_goal_directive: &NativeBridge,
    submission: PromptSubmitted,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = submission.session_id.clone();
    let planning_input = {
        let mut sessions = goal_sessions
            .lock()
            .map_err(|_| "goal sessions lock failed")?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or("MICE received a goal prompt for an unknown session.")?;
        match session.state() {
            GoalState::AwaitingGoal => {
                session.submit_goal(submission.text.clone())?;
                submission.text
            }
            GoalState::Reviewing { .. } if submission.text.trim().is_empty() => {
                let mut writer = agent_writer
                    .lock()
                    .map_err(|_| "agent writer lock failed")?;
                return send_command(
                    &mut writer,
                    AgentCommand::OverlayResult {
                        session_id: submission.session_id,
                        actions: goal_review_actions(),
                    },
                );
            }
            GoalState::Reviewing { .. } => session.begin_revision(submission.text)?,
            GoalState::Planning { .. } => {
                return Err("MICE is already generating this plan.".into());
            }
            GoalState::Accepted { .. } | GoalState::Cancelled => {
                return Err("This goal session is already finished.".into());
            }
        }
    };

    if let Ok(mut writer) = agent_writer.lock() {
        let _ = send_command(
            &mut writer,
            AgentCommand::OverlayShow {
                text: "Planning your goal…".into(),
            },
        );
    }

    let config = config.clone();
    let agent_writer = Arc::clone(agent_writer);
    let goal_sessions = Arc::clone(goal_sessions);
    let goal_plans = Arc::clone(goal_plans);
    let active_guides = Arc::clone(active_guides);
    let browser_goal_directive = Arc::clone(browser_goal_directive);

    std::thread::spawn(move || {
        let plan = match generate_goal_plan(&config, &planning_input) {
            Ok(plan) => plan,
            Err(error) => {
                let message = format!(
                    "MICE could not make this plan: {error}\n\nCheck your selected provider in `mice settings`, then try again."
                );
                if let Ok(mut sessions) = goal_sessions.lock() {
                    sessions.remove(&session_id);
                }
                if let Ok(mut plans) = goal_plans.lock() {
                    plans.remove(&session_id);
                }
                if let Ok(mut guides) = active_guides.lock() {
                    guides.remove(&session_id);
                }
                clear_browser_goal_directive(&browser_goal_directive);
                if let Ok(mut writer) = agent_writer.lock() {
                    let _ = send_command(
                        &mut writer,
                        AgentCommand::OverlayFinishResult {
                            text: Some(message),
                        },
                    );
                }
                return;
            }
        };

        let rendered = render_goal_plan(&plan);
        if let Ok(mut sessions) = goal_sessions.lock()
            && let Some(session) = sessions.get_mut(&session_id)
        {
            let _ = session.review(rendered.clone());
        }
        if let Ok(mut plans) = goal_plans.lock() {
            plans.insert(session_id.clone(), plan);
        }

        record_user_history(
            memory::HistoryKind::GoalPlan,
            &planning_input,
            &rendered,
            None,
        );

        if let Ok(mut writer) = agent_writer.lock() {
            let _ = send_command(&mut writer, AgentCommand::OverlayShow { text: rendered });
            let _ = send_command(
                &mut writer,
                AgentCommand::OverlayResult {
                    session_id,
                    actions: goal_review_actions(),
                },
            );
        }
    });

    Ok(())
}

/// Dispatch the small, explicit palette surface. It intentionally reuses the
/// same typed selection and goal flows as global gestures: the palette is a
/// faster entry point, never a second decision-maker or a background observer.
fn handle_palette_submission(
    agent_writer: &Arc<Mutex<ChildStdin>>,
    config: &mice_core::Config,
    sessions: &Arc<Mutex<HashMap<String, GoalSession>>>,
    goal_plans: &Arc<Mutex<HashMap<String, GoalPlanResult>>>,
    active_guides: &Arc<Mutex<HashMap<String, ActiveGuide>>>,
    browser_goal_directive: &NativeBridge,
    submission: PaletteSubmitted,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = submission.session_id;
    let intent = parse_palette_intent(&submission.text);
    // Questions remain Ask requests, but an unmistakable task statement is
    // allowed to enter the reviewed Goal Guide without teaching people a
    // command language. This does not authorize execution: the generated plan
    // still needs an explicit Start guide decision.
    let plan_goal = match &intent {
        PaletteIntent::Plan(goal) => Some(goal.clone()),
        PaletteIntent::Ask(question) if looks_like_goal_statement(question) => {
            Some(question.clone())
        }
        _ => None,
    };
    if let Some(goal) = plan_goal {
        if goal.trim().is_empty() {
            let mut writer = agent_writer
                .lock()
                .map_err(|_| "agent writer lock failed")?;
            return palette_finish(&mut writer, &session_id, "Type a goal after `plan`.");
        }
        // Only one reviewed plan may be resumed by the Goal gesture. A new
        // palette plan intentionally supersedes an older unstarted review,
        // rather than allowing HashMap iteration order to choose a plan.
        {
            let mut sessions_guard = sessions.lock().map_err(|_| "sessions lock failed")?;
            let mut plans_guard = goal_plans.lock().map_err(|_| "plans lock failed")?;
            let mut guides_guard = active_guides.lock().map_err(|_| "guides lock failed")?;
            sessions_guard.retain(|existing_id, session| {
                let keep = !matches!(session.state(), GoalState::Reviewing { .. });
                if !keep {
                    plans_guard.remove(existing_id);
                    guides_guard.remove(existing_id);
                }
                keep
            });
            sessions_guard.insert(session_id.clone(), GoalSession::new());
        }
        if let Ok(mut writer) = agent_writer.lock() {
            let _ = send_command(
                &mut writer,
                AgentCommand::PaletteDismiss {
                    session_id: session_id.clone(),
                },
            );
        }
        return handle_goal_submission(
            agent_writer,
            config,
            sessions,
            goal_plans,
            active_guides,
            browser_goal_directive,
            PromptSubmitted {
                session_id,
                text: goal,
            },
        );
    }
    let mut writer_guard = agent_writer
        .lock()
        .map_err(|_| "agent writer lock failed")?;
    let writer = &mut *writer_guard;
    match intent {
        PaletteIntent::Plan(_) => unreachable!("handled before palette dispatch"),
        PaletteIntent::Summarize(_) => {
            let Some(text) = submission
                .selection_text
                .filter(|text| !text.trim().is_empty())
            else {
                return palette_finish(
                    writer,
                    &session_id,
                    "Select text first, then use summarize.",
                );
            };
            send_command(
                writer,
                AgentCommand::PaletteDismiss {
                    session_id: session_id.clone(),
                },
            )?;
            let _ = handle_selection_action(
                writer,
                config,
                SelectionText {
                    session_id,
                    text,
                    html: None,
                    source: mice_ipc::SelectionSource::Ax,
                    action: SelectionAction::Summarize,
                },
            )?;
            Ok(())
        }
        PaletteIntent::Define(term) => {
            // A typed `define` term is intentional and always wins. A
            // selection is a convenience fallback only for bare `define`.
            let text = (!term.trim().is_empty())
                .then_some(term)
                .or_else(|| {
                    submission
                        .selection_text
                        .filter(|text| !text.trim().is_empty())
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                return palette_finish(
                    writer,
                    &session_id,
                    "Type a word after `define`, or select text first.",
                );
            }
            send_command(
                writer,
                AgentCommand::PaletteDismiss {
                    session_id: session_id.clone(),
                },
            )?;
            let _ = handle_selection_action(
                writer,
                config,
                SelectionText {
                    session_id,
                    text,
                    html: None,
                    source: mice_ipc::SelectionSource::Ax,
                    action: SelectionAction::Define,
                },
            )?;
            Ok(())
        }
        PaletteIntent::Remember(note) => {
            user_history()?.remember(&note)?;
            palette_finish(writer, &session_id, "Saved as a local MICE preference.")
        }
        PaletteIntent::History(query) => {
            let entries = user_history()?.search((!query.is_empty()).then_some(query.as_str()))?;
            let text = if entries.is_empty() {
                "No matching MICE history.".into()
            } else {
                entries
                    .into_iter()
                    .take(12)
                    .map(|entry| format!("{}: {}", entry.question, entry.answer_digest))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            palette_finish(writer, &session_id, &text)
        }
        PaletteIntent::Schedule(args) => {
            let now = memory::now();
            match parse_schedule_intent(&args, now) {
                Ok(task) => {
                    let store = schedule_store()?;
                    store.add(task.clone())?;
                    let trigger_in = task.trigger_at.saturating_sub(now);
                    palette_finish(
                        writer,
                        &session_id,
                        &format!("Scheduled task `{}` due in {trigger_in}s.", task.id),
                    )
                }
                Err(err) => palette_finish(writer, &session_id, &format!("Schedule error: {err}")),
            }
        }
        PaletteIntent::Remind(args) => {
            let now = memory::now();
            match parse_remind_intent(&args, now) {
                Ok(task) => {
                    let store = schedule_store()?;
                    store.add(task.clone())?;
                    let trigger_in = task.trigger_at.saturating_sub(now);
                    palette_finish(
                        writer,
                        &session_id,
                        &format!("Reminder set for {trigger_in}s from now."),
                    )
                }
                Err(err) => palette_finish(writer, &session_id, &format!("Reminder error: {err}")),
            }
        }
        PaletteIntent::Ask(question) => handle_palette_ask(writer, config, &session_id, &question),
        PaletteIntent::See(_)
        | PaletteIntent::Sheet(_)
        | PaletteIntent::Tidy(_)
        | PaletteIntent::File(_) => palette_finish(
            writer,
            &session_id,
            "That command is available from the CLI for now. Palette support for it is coming next.",
        ),
    }
}

fn palette_finish(
    writer: &mut ChildStdin,
    session_id: &str,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    send_command(
        writer,
        AgentCommand::PaletteFinishResult {
            session_id: session_id.into(),
            text: Some(text.into()),
        },
    )
}

fn schedule_store() -> Result<crate::memory::ScheduleStore, Box<dyn std::error::Error>> {
    let root = crate::memory::ScheduleStore::default_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "MICE schedule directory is unavailable",
        )
    })?;
    Ok(crate::memory::ScheduleStore::at(root)?)
}

fn parse_schedule_intent(args: &str, now: u64) -> Result<ScheduledTask, String> {
    let args = args.trim();
    if args.is_empty() {
        return Err("Usage: schedule <goal> in <time> (e.g. schedule build app in 10m)".into());
    }
    let (goal, time_str) = if let Some((g, t)) = args.rsplit_once(" in ") {
        (g.trim(), t.trim())
    } else if let Some((g, t)) = args.rsplit_once(" at ") {
        (g.trim(), t.trim())
    } else {
        (args, "10m")
    };

    let trigger_at = parse_schedule_time(time_str, now)?;
    let id = format!(
        "sched-{}",
        memory::digest_name(goal).get(..8).unwrap_or("0000")
    );
    Ok(ScheduledTask {
        id,
        created_at: now,
        trigger_at,
        cron_expression: None,
        action: ScheduleAction::ExecuteGoal {
            goal: goal.into(),
            plan: None,
        },
        triggered: false,
    })
}

fn parse_remind_intent(args: &str, now: u64) -> Result<ScheduledTask, String> {
    let args = args.trim();
    if args.is_empty() {
        return Err(
            "Usage: remind me in <time> to <msg> (e.g. remind me in 30m to check PR)".into(),
        );
    }
    let text = args.trim_start_matches("me ").trim();
    let (time_str, message) = if let Some((t, m)) = text.split_once(" to ") {
        (t.trim_start_matches("in ").trim(), m.trim())
    } else if let Some((g, t)) = text.rsplit_once(" in ") {
        (t.trim(), g.trim())
    } else {
        ("10m", text)
    };

    let trigger_at = parse_schedule_time(time_str, now)?;
    let id = format!(
        "remind-{}",
        memory::digest_name(message).get(..8).unwrap_or("0000")
    );
    Ok(ScheduledTask {
        id,
        created_at: now,
        trigger_at,
        cron_expression: None,
        action: ScheduleAction::Reminder {
            message: message.into(),
        },
        triggered: false,
    })
}

fn schedule_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(2).collect();
    let sub = args.first().map(String::as_str);
    let store = schedule_store()?;
    let now = memory::now();

    match sub {
        Some("list") | None => {
            let tasks = store.list()?;
            if tasks.is_empty() {
                println!("No scheduled tasks or reminders.");
                return Ok(());
            }
            println!("MICE Scheduled Tasks & Reminders:\n");
            println!(
                "{:<16} {:<14} {:<10} {:<30}",
                "ID", "TRIGGER IN", "STATUS", "ACTION"
            );
            println!("{}", "-".repeat(72));
            for task in tasks {
                let status = if task.triggered {
                    "triggered"
                } else {
                    "pending"
                };
                let rel = if task.trigger_at > now {
                    format!("{}s", task.trigger_at - now)
                } else {
                    "due".to_owned()
                };
                let desc = match task.action {
                    ScheduleAction::Reminder { message } => format!("Reminder: {message}"),
                    ScheduleAction::ExecuteGoal { goal, .. } => format!("Goal: {goal}"),
                };
                println!("{:<16} {:<14} {:<10} {:<30}", task.id, rel, status, desc);
            }
        }
        Some("add") => {
            let time_arg = args
                .windows(2)
                .find(|w| w[0] == "--in" || w[0] == "--at")
                .map(|w| w[1].as_str())
                .unwrap_or("10m");
            let goal_arg = args
                .windows(2)
                .find(|w| w[0] == "--goal")
                .map(|w| w[1].as_str());
            let remind_arg = args
                .windows(2)
                .find(|w| w[0] == "--reminder")
                .map(|w| w[1].as_str());

            let trigger_at = parse_schedule_time(time_arg, now)?;
            let (action, id_prefix) = if let Some(g) = goal_arg {
                (
                    ScheduleAction::ExecuteGoal {
                        goal: g.into(),
                        plan: None,
                    },
                    "sched",
                )
            } else if let Some(r) = remind_arg {
                (ScheduleAction::Reminder { message: r.into() }, "remind")
            } else {
                return Err("Specify `--goal \"...\"` or `--reminder \"...\"`.".into());
            };
            let id = format!(
                "{id_prefix}-{}",
                memory::digest_name(&format!("{trigger_at}"))
                    .get(..8)
                    .unwrap_or("0000")
            );
            let task = ScheduledTask {
                id: id.clone(),
                created_at: now,
                trigger_at,
                cron_expression: None,
                action,
                triggered: false,
            };
            store.add(task)?;
            println!("Scheduled task `{id}` for {time_arg} from now.");
        }
        Some("cancel") => {
            let id = args.get(1).ok_or("Usage: mice schedule cancel <id>")?;
            if store.cancel(id)? {
                println!("Cancelled scheduled task `{id}`.");
            } else {
                println!("No active scheduled task with ID `{id}`.");
            }
        }
        _ => {
            println!(
                "Usage:\n  mice schedule list\n  mice schedule add [--in <time>] [--goal \"...\" | --reminder \"...\"]\n  mice schedule cancel <id>"
            );
        }
    }
    Ok(())
}

fn handle_palette_ask(
    writer: &mut ChildStdin,
    config: &mice_core::Config,
    session_id: &str,
    question: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if question.trim().is_empty() {
        return palette_finish(writer, session_id, "Type a question for MICE.");
    }
    let preferences = user_history()
        .ok()
        .and_then(|history| history.preferences_preamble().ok().flatten());
    let instruction = apply_preferences(question, preferences.as_deref());
    let request = RouteRequest {
        artifacts: Artifacts::default(),
        instruction: instruction.clone(),
        action: Some(Action::Summarize),
        privacy_mode: config.privacy_mode,
        cost_policy: config.cost_policy,
        model_preferences: ModelPreferences {
            local_model: config.local_model.clone(),
            cloud_model: config.cloud_model.clone(),
        },
    };
    let selected = route(&request)?.model;
    let mut stream = PaletteStream::new(writer, session_id);
    if selected.locality == mice_providers::Locality::Local {
        let _ = ensure_ollama_server();
        mice_providers::ollama_model_ready("http://127.0.0.1:11434", selected.id)?;
        stream_ollama(selected.id, &instruction, None, |chunk| stream.push(chunk))?;
    } else if is_groq_model(selected.id) {
        stream_groq(selected.id, &instruction, None, |chunk| stream.push(chunk))?;
    } else {
        stream_openai(selected.id, &instruction, None, |chunk| stream.push(chunk))?;
    }
    let answer = stream.finish()?;
    if answer.trim().is_empty() {
        return palette_finish(
            writer,
            session_id,
            "MICE received an empty response. Check `mice status` and your selected model, then try again.",
        );
    }
    record_user_history(memory::HistoryKind::Palette, question, &answer, None);
    palette_finish(writer, session_id, "")
}

/// Keep ordinary questions such as “OpenAI pricing” and “help me understand
/// Rust” as Ask requests, while letting a concrete multi-step task feel
/// natural in the palette. The classifier is intentionally narrow and merely
/// chooses the reviewed planning UI; it can never cause browser mutation.
fn looks_like_goal_statement(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    let starts_with_destination = ["go to ", "navigate to ", "visit "]
        .iter()
        .any(|prefix| value.starts_with(prefix));
    let starts_with_creation = ["create ", "make ", "set up ", "sign up ", "register "]
        .iter()
        .any(|prefix| value.starts_with(prefix));
    let has_follow_on_action = [
        " start ",
        " create ",
        " choose ",
        " design",
        " upload ",
        " fill ",
        " organize ",
    ]
    .iter()
    .any(|needle| value.contains(needle));
    // "Plan"/"planning" is the single most explicit signal a person can give
    // short of typing the literal `plan` verb, and is otherwise checked
    // nowhere in this classifier. Word-bounded (not `.contains`) so it
    // doesn't fire on unrelated words like "explanation".
    let mentions_plan = value
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == "plan" || word == "planning" || word == "plans");
    starts_with_creation || mentions_plan || (starts_with_destination && has_follow_on_action)
}

fn goal_review_actions() -> Vec<mice_ipc::OverlayAction> {
    vec![
        mice_ipc::OverlayAction {
            id: "goal.accept".into(),
            label: "Start guide".into(),
        },
        mice_ipc::OverlayAction {
            id: "goal.revise".into(),
            label: "Revise".into(),
        },
        mice_ipc::OverlayAction {
            id: "goal.cancel".into(),
            label: "Cancel".into(),
        },
    ]
}

/// Re-open the most recent reviewed plan after its overlay was dismissed.
/// The plan text lives in `GoalSession`, so no provider call or regeneration
/// is needed merely because the person changed applications.
fn resume_reviewed_goal(
    writer: &mut ChildStdin,
    sessions: &HashMap<String, GoalSession>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some((session_id, plan, is_planning)) =
        sessions
            .iter()
            .find_map(|(session_id, session)| match session.state() {
                GoalState::Reviewing { plan, .. } => {
                    Some((session_id.clone(), plan.clone(), false))
                }
                GoalState::Planning { .. } => {
                    Some((session_id.clone(), "Planning your goal…".to_string(), true))
                }
                _ => None,
            })
    else {
        return Ok(false);
    };
    send_command(writer, AgentCommand::OverlayShow { text: plan })?;
    if !is_planning {
        send_command(
            writer,
            AgentCommand::OverlayResult {
                session_id,
                actions: goal_review_actions(),
            },
        )?;
    }
    Ok(true)
}

fn handle_goal_review_action(
    writer: &mut ChildStdin,
    sessions: &mut HashMap<String, GoalSession>,
    goal_plans: &mut HashMap<String, GoalPlanResult>,
    active_guides: &mut HashMap<String, ActiveGuide>,
    browser_goal_directive: &NativeBridge,
    session_id: &str,
    action: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if action == "goal.cancel" {
        let session = sessions
            .get_mut(session_id)
            .ok_or("MICE received a plan action for an unknown session.")?;
        if !matches!(session.state(), GoalState::Reviewing { .. }) {
            return Err("This plan is no longer awaiting review.".into());
        }
        session.cancel()?;
        // Plans and their original goals are transient daemon state. Once a
        // person cancels, retain neither for the rest of the daemon lifetime.
        sessions.remove(session_id);
        goal_plans.remove(session_id);
        return send_command(
            writer,
            AgentCommand::OverlayFinishResult {
                text: Some("Goal planning cancelled.".into()),
            },
        );
    }
    let session = sessions
        .get_mut(session_id)
        .ok_or("MICE received a plan action for an unknown session.")?;
    if !matches!(session.state(), GoalState::Reviewing { .. }) {
        return Err("This plan is no longer awaiting review.".into());
    }
    match action {
        "goal.accept" => {
            session.accept()?;
            let plan = goal_plans
                .get(session_id)
                .ok_or("MICE lost the reviewed plan; please start again.")?;
            active_guides.insert(
                session_id.into(),
                ActiveGuide {
                    steps: plan.steps.clone(),
                    current_step: 0,
                },
            );
            show_active_guide_step(writer, active_guides, browser_goal_directive, session_id)
        }
        "goal.revise" => send_command(
            writer,
            AgentCommand::OverlayPromptInput {
                session_id: session_id.into(),
                title: "Revise your plan".into(),
                placeholder: "What should MICE change?".into(),
                context: Some("Tell MICE what to add, remove, or simplify. Your current plan stays visible behind this prompt.".into()),
            },
        ),
        _ => Err("Unknown plan review action.".into()),
    }
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
        let _ = ensure_ollama_server();
        if mice_providers::ollama_model_ready("http://127.0.0.1:11434", selected.id).is_ok() {
            let mut response = String::new();
            if stream_ollama(
                selected.id,
                "Return only JSON with 3-8 advisory steps. Each step needs instruction, app_hint, and sensitive boolean.",
                Some(planning_input),
                |chunk| {
                    response.push_str(chunk);
                    Ok(())
                },
            )
            .is_ok()
                && !response.trim().is_empty()
            {
                response
            } else {
                return Ok(local_goal_plan_recovery(planning_input));
            }
        } else {
            return Ok(local_goal_plan_recovery(planning_input));
        }
    } else if is_groq_model(selected.id) {
        call_groq_goal_plan(selected.id, planning_input)?
    } else {
        call_openai_goal_plan(selected.id, planning_input)?
    };
    match parse_goal_plan(&raw) {
        Ok(plan) => Ok(plan),
        // Gemma and other local models commonly add a Markdown fence or an
        // explanatory sentence around otherwise-valid JSON. `parse_goal_plan`
        // handles the common cases; if this small formatting-only contract is
        // still missed, give the person a clearly safe starter plan rather
        // than leaving the Goal UI blank. An unavailable Ollama server/model
        // returned above as an error and is never hidden by this recovery.
        Err(_) if selected.locality == mice_providers::Locality::Local => {
            Ok(local_goal_plan_recovery(planning_input))
        }
        Err(error) => Err(error),
    }
}

fn parse_goal_plan(raw: &str) -> Result<GoalPlanResult, Box<dyn std::error::Error>> {
    let raw = strip_markdown_fence(raw);
    let plan: GoalPlanResult = serde_json::from_str(extract_json_object(raw))
        .map_err(|_| "The planning model returned an invalid plan; please try again.")?;
    validate_goal_plan(&plan)?;
    Ok(plan)
}

/// Local models should normally produce the same structured plan as cloud
/// models. This recovery covers only malformed presentation around a completed
/// local response, preserving the core safety boundary: it gives advice and
/// never emits executable browser actions.
fn local_goal_plan_recovery(goal: &str) -> GoalPlanResult {
    let goal = bounded_for_model(goal.trim(), 180);
    let app_hint = infer_goal_app_hint(&goal);
    GoalPlanResult {
        steps: vec![
            GoalPlanStep {
                instruction: format!("Open the app or website you need for: {goal}"),
                app_hint: app_hint.clone(),
                sensitive: false,
            },
            GoalPlanStep {
                instruction: "Find the relevant page, template, or starting point for your goal.".into(),
                app_hint: app_hint.clone(),
                sensitive: false,
            },
            GoalPlanStep {
                instruction: "Carry out the next step yourself, then review the result before sharing, submitting, or paying.".into(),
                app_hint,
                sensitive: false,
            },
        ],
    }
}

fn infer_goal_app_hint(goal: &str) -> String {
    let goal = goal.to_ascii_lowercase();
    if goal.contains("canva") {
        "Canva in your browser".into()
    } else if goal.contains("chrome") || goal.contains("browser") || goal.contains("website") {
        "Your web browser".into()
    } else {
        "The relevant app or website".into()
    }
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
            presentation: Some("panel".into()),
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
) -> Result<bool, Box<dyn std::error::Error>> {
    if action == "do-it" || action == "do-it-fill" {
        send_command(
            writer,
            AgentCommand::OverlayFinishResult {
                text: Some("Goal Guide only highlights and explains. Use `mice autopilot --engine axi <goal>` for individually confirmed browser actions.".into()),
            },
        )?;
        return Ok(false);
    }
    if action == "quit" {
        guides.remove(session_id);
        clear_browser_goal_directive(browser_goal_directive);
        send_command(writer, AgentCommand::OverlayHighlight { boxes: vec![] })?;
        send_command(
            writer,
            AgentCommand::OverlayFinishResult {
                text: Some("Goal Guide ended. You can start another goal at any time.".into()),
            },
        )?;
        return Ok(true);
    }
    let guide = guides
        .get_mut(session_id)
        .ok_or("No active guide was found for this session.")?;
    match action {
        "next" if guide.current_step + 1 == guide.steps.len() => {
            guides.remove(session_id);
            clear_browser_goal_directive(browser_goal_directive);
            send_command(writer, AgentCommand::OverlayHighlight { boxes: vec![] })?;
            send_command(
                writer,
                AgentCommand::OverlayFinishResult {
                    text: Some("Goal Guide complete. Great work!".into()),
                },
            )?;
            return Ok(true);
        }
        "next" => guide.current_step += 1,
        "back" => guide.current_step = guide.current_step.saturating_sub(1),
        "stay" => {}
        _ => return Err("Unknown guide control.".into()),
    }
    show_active_guide_step(writer, guides, browser_goal_directive, session_id)?;
    Ok(false)
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
    if is_terminal_command_field(hover.app_name.as_deref(), &label, ax.role.as_deref()) {
        send_command(
            writer,
            AgentCommand::OverlayShow {
                text: "Terminal command area. Type a shell command here, then press Return to run it; output appears in this same window.".into(),
            },
        )?;
        return send_command(writer, AgentCommand::OverlayFinishResult { text: None });
    }
    let control_type = hover_control_type(ax.role.as_deref(), Some(&label), None);
    let auxiliary_context = auxiliary_hover_context(ax.description.as_deref(), &label);
    let current_value = ax.value.unwrap_or_default();
    let actions = ax.actions.join(", ");
    let context = format!(
        "Current control (captured under the pointer now):\nApplication: {}\nControl type: {control_type}\nVisible label: {}\nAdditional context: {}\nCurrent value: {}\nAvailable actions: {}",
        bounded_for_model(hover.app_name.as_deref().unwrap_or("the current app"), 120),
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
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        if selected.locality == mice_providers::Locality::Local {
            mice_providers::ollama_model_ready("http://127.0.0.1:11434", selected.id)?;
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
        }
    })();
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

fn is_terminal_command_field(app_name: Option<&str>, label: &str, role: Option<&str>) -> bool {
    let app = app_name.unwrap_or_default().to_ascii_lowercase();
    let label = label.to_ascii_lowercase();
    let role = role.unwrap_or_default().to_ascii_lowercase();
    (app == "terminal" || app.contains("iterm"))
        && (label.contains("shell text") || label == "input field" || role.contains("textfield"))
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
    spawn_agent(gesture, false, false, false, false, false)
}

/// The resident daemon's agent: the only agent allowed to open the palette.
/// Probe agents (`mice status`) and one-shot fallbacks must never present a
/// palette from a stray gesture while they briefly exist.
fn start_agent_daemon(
    gesture: &mice_core::GestureConfig,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    spawn_agent(gesture, false, false, true, false, false)
}

/// A display-only agent for one-shot commands: it never creates an event tap,
/// so it needs no Input Monitoring grant and observes no input.
fn start_agent_overlay_only(
    gesture: &mice_core::GestureConfig,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    spawn_agent(gesture, false, true, false, false, false)
}

fn start_agent_home(
    gesture: &mice_core::GestureConfig,
    resident_daemon: bool,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    spawn_agent(gesture, false, true, false, true, resident_daemon)
}

/// Ask the thin macOS agent for the existing Finder selection. The core only
/// receives the path after the explicit `mice file --finder` command; it never
/// watches Finder or persists the selection.
/// How long `mice file --finder` waits for the agent. Generous because the
/// first use can show a macOS Automation permission prompt the person has to
/// approve; a stuck agent must still never hang the command forever.
const FINDER_CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn capture_finder_file(
    gesture: &mice_core::GestureConfig,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let agent = start_agent_overlay_only(gesture)?;
    let session_id = format!("finder-{}", std::process::id());
    let AgentSession {
        mut child,
        mut stdin,
        reader,
        ..
    } = agent;
    let outcome = (|| -> Result<PathBuf, Box<dyn std::error::Error>> {
        send_command(
            &mut stdin,
            AgentCommand::FinderCapture {
                session_id: session_id.clone(),
            },
        )?;
        println!(
            "Reading the Finder selection (approve the macOS Automation prompt if it appears)…"
        );
        let (sender, receiver) = mpsc::channel();
        let expected = session_id.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            loop {
                let Ok(message) = read_frame::<mice_ipc::RpcNotification>(&mut reader) else {
                    return;
                };
                if message.method != "finder.captured" {
                    continue;
                }
                let Ok(captured) =
                    serde_json::from_value::<mice_ipc::FinderCaptured>(message.params)
                else {
                    return;
                };
                if captured.session_id == expected {
                    let _ = sender.send(captured);
                    return;
                }
            }
        });
        let captured = receiver.recv_timeout(FINDER_CAPTURE_TIMEOUT).map_err(|_| {
            format!(
                "Timed out after {} seconds waiting for the Finder selection. If macOS showed an Automation permission prompt, approve it and run `mice file --finder` again.",
                FINDER_CAPTURE_TIMEOUT.as_secs()
            )
        })?;
        if let Some(error) = captured.capture_error {
            return Err(error.into());
        }
        // The protocol is exactly one selected file. Anything else is a
        // malformed or incompatible agent response; silently taking the
        // first path could file an unintended item.
        if captured.paths.len() != 1 {
            return Err(format!(
                "The agent returned {} paths where exactly one Finder selection was expected; nothing was filed.",
                captured.paths.len()
            )
            .into());
        }
        let path = captured
            .paths
            .into_iter()
            .next()
            .expect("length was checked above");
        Ok(PathBuf::from(path))
    })();
    // The one-shot agent may be stuck inside the AppleScript call after a
    // timeout; end it explicitly rather than waiting on EOF behavior.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

fn spawn_agent(
    gesture: &mice_core::GestureConfig,
    autopilot_active: bool,
    overlay_only: bool,
    daemon: bool,
    home_only: bool,
    home_has_resident_daemon: bool,
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
        .env("MICE_PALETTE_TRIGGER", &gesture.palette_trigger)
        .env("MICE_DAEMON", if daemon { "1" } else { "0" })
        .env("MICE_HOME_ONLY", if home_only { "1" } else { "0" })
        .env(
            "MICE_HOME_HAS_RESIDENT_DAEMON",
            if home_has_resident_daemon { "1" } else { "0" },
        )
        .env(
            "MICE_AUTOPILOT_ACTIVE",
            if autopilot_active { "1" } else { "0" },
        )
        .env("MICE_OVERLAY_ONLY", if overlay_only { "1" } else { "0" })
        .env(
            "MICE_EXCLUDE_PIDS",
            launch_chain_pids()
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .env(
            "MICE_EXCLUDE_BUNDLES",
            terminal_host_bundle_prefixes().join(","),
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

/// The process chain that launched this command: mice itself, its shell, and
/// the terminal application above them. Running `mice see` necessarily makes
/// that terminal frontmost, so the agent excludes these processes' windows
/// when choosing the "front" window to capture.
fn launch_chain_pids() -> Vec<u32> {
    let mut pids = vec![std::process::id()];
    let mut current = std::process::id();
    for _ in 0..6 {
        let Ok(output) = Command::new("/bin/ps")
            .args(["-o", "ppid=", "-p", &current.to_string()])
            .output()
        else {
            break;
        };
        let Ok(parent) = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
        else {
            break;
        };
        if parent <= 1 {
            break;
        }
        pids.push(parent);
        current = parent;
    }
    pids
}

/// Terminal-host applications are excluded from implicit front-window screen
/// capture even when the invoking shell is detached from them (tmux, SSH, or
/// an IDE-integrated terminal). A caller can still request an explicit display
/// capture, which performs its own credential-manager check.
fn terminal_host_bundle_prefixes() -> Vec<&'static str> {
    let mut prefixes = vec![
        "com.apple.terminal",
        "com.googlecode.iterm2",
        "dev.warp",
        "net.kovidgoyal.kitty",
        "org.alacritty",
        "com.github.wez.wezterm",
        "co.zeit.hyper",
        "com.mitchellh.ghostty",
    ];
    match std::env::var("TERM_PROGRAM").ok().as_deref() {
        Some("vscode") => prefixes.push("com.microsoft.vscode"),
        Some("vscode-insiders") => prefixes.push("com.microsoft.vscodeinsiders"),
        Some("JetBrains-JediTerm") => prefixes.push("com.jetbrains."),
        _ => {}
    }
    prefixes
}

fn agent_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os("MICE_MAC_AGENT_PATH") {
        return Ok(PathBuf::from(path));
    }

    // A packaged install (MICE.app/Contents/MacOS) ships the agent beside the
    // CLI binary, so upgrades replace both together and never mix versions.
    // `current_exe()` returns the path as invoked, not resolved through
    // symlinks (macOS `_NSGetExecutablePath` semantics) — `mice install`'s
    // own launcher at `~/.local/bin/mice` is exactly such a symlink, so this
    // must canonicalize first or every install-launched run silently misses
    // its bundled sibling and falls through to the dev-workspace fallback.
    if let Ok(executable) = env::current_exe()
        && let Ok(executable) = std::fs::canonicalize(&executable)
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
                        "instructions": "You are paired with MICE, a local execution manager. Delegate mechanical or token-heavy work through delegate_task or run_tool; MICE first uses deterministic local CLIs and returns bounded results. Check mission_status before editing an assigned mission task, check team_status before editing shared files, and record durable decisions with memory_note."
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
        json!({"name": "mission_status", "description": "Read the active MICE mission's task ownership, lifecycle state, and bounded Git overlap warnings. It never launches, merges, or edits.", "inputSchema": {"type": "object", "properties": {"plan_path": {"type": "string"}}, "required": ["plan_path"]}}),
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
        "mission_status" => mission::mcp_status(string_argument("plan_path")?),
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
    ensure_local_model(config)?;
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
    let model_instruction = action_instruction(action, &instruction);
    let preferences = user_history()
        .ok()
        .and_then(|history| history.preferences_preamble().ok().flatten());
    let request = RouteRequest {
        artifacts: Artifacts {
            text: text.clone(),
            ..Default::default()
        },
        instruction: apply_preferences(&model_instruction, preferences.as_deref()),
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
                if text.is_some() {
                    // The answer can quote piped private source text verbatim.
                    // Keep the useful request marker but never retain a
                    // source-derived response in local history.
                    record_sensitive_history(memory::HistoryKind::Ask, &instruction, None);
                } else {
                    record_user_history(memory::HistoryKind::Ask, &instruction, &response, None);
                }
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
    let scope = match arguments.first().map(String::as_str) {
        Some("--display") => {
            arguments.remove(0);
            mice_ipc::ScreenCaptureScope::DisplayUnderMouse
        }
        // Detail mode: native-resolution tiled OCR for dense small text such
        // as spreadsheets; the outbound image stays bounded.
        Some("--sheet") => {
            arguments.remove(0);
            mice_ipc::ScreenCaptureScope::FrontWindowDetail
        }
        _ => mice_ipc::ScreenCaptureScope::FrontWindow,
    };
    let question = arguments.join(" ");
    if question.is_empty() {
        return Err("Usage: mice see [--display|--sheet] <question about your screen>".into());
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
        && provider_api_key("OPENAI_API_KEY").is_ok()
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
        let api_key = provider_api_key("OPENAI_API_KEY")?;
        let data_url = format!(
            "data:image/png;base64,{}",
            captured.png_base64.unwrap_or_default()
        );
        let payload = mice_providers::openai_vision_answer_payload(
            model,
            &question,
            &bounded_for_model(
                &ocr_text,
                if scope == mice_ipc::ScreenCaptureScope::FrontWindowDetail {
                    8_000
                } else {
                    4_000
                },
            ),
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
        record_sensitive_history(
            memory::HistoryKind::See,
            &question,
            captured.app_name.clone(),
        );
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
    let instruction = apply_preferences(
        &instruction,
        user_history()
            .ok()
            .and_then(|history| history.preferences_preamble().ok().flatten())
            .as_deref(),
    );
    let mut stream = OverlayStream::echoing(&mut agent.stdin);
    stream_ollama(
        &config.local_model,
        &instruction,
        Some(&bounded_for_model(
            &ocr_text,
            if scope == mice_ipc::ScreenCaptureScope::FrontWindowDetail {
                24_000
            } else {
                12_000
            },
        )),
        |chunk| stream.push(chunk),
    )?;
    let answer = stream.finish()?;
    println!();
    if !answer.is_empty() {
        send_command(&mut agent.stdin, clipboard_command(&answer))?;
        record_sensitive_history(
            memory::HistoryKind::See,
            &question,
            captured.app_name.clone(),
        );
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
    on_chunk: impl FnMut(&str) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    stream_ollama_with_format(model, instruction, text, None, on_chunk)
}

fn stream_ollama_with_format(
    model: &str,
    instruction: &str,
    text: Option<&str>,
    format: Option<serde_json::Value>,
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
        format,
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

/// Palette output has the same backpressure characteristics as overlay
/// streaming, but it is also bounded: a bad or unusually verbose provider
/// response cannot grow the daemon or native text view indefinitely.
struct PaletteStream<'a> {
    writer: &'a mut ChildStdin,
    session_id: &'a str,
    response: String,
    pending: String,
    response_chars: usize,
    last_flush: Instant,
    truncated: bool,
}

impl<'a> PaletteStream<'a> {
    const FLUSH_BYTES: usize = 512;
    const FLUSH_INTERVAL: Duration = Duration::from_millis(80);
    const MAX_RESULT_CHARS: usize = 12_000;
    const TRUNCATION_NOTICE: &'static str = "\n\n[Result truncated at 12,000 characters.]";

    fn response_budget() -> usize {
        Self::MAX_RESULT_CHARS - Self::TRUNCATION_NOTICE.chars().count()
    }

    fn new(writer: &'a mut ChildStdin, session_id: &'a str) -> Self {
        Self {
            writer,
            session_id,
            response: String::new(),
            pending: String::new(),
            response_chars: 0,
            last_flush: Instant::now(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &str) -> Result<(), Box<dyn std::error::Error>> {
        let remaining = Self::response_budget().saturating_sub(self.response_chars);
        if remaining == 0 {
            self.truncated = true;
            return Ok(());
        }
        let bounded = chunk.chars().take(remaining).collect::<String>();
        let bounded_chars = bounded.chars().count();
        if bounded_chars < chunk.chars().count() {
            self.truncated = true;
        }
        self.response_chars += bounded_chars;
        self.response.push_str(&bounded);
        self.pending.push_str(&bounded);
        if self.pending.len() >= Self::FLUSH_BYTES
            || self.last_flush.elapsed() >= Self::FLUSH_INTERVAL
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.pending.is_empty() {
            send_command(
                self.writer,
                AgentCommand::PaletteAppendResult {
                    session_id: self.session_id.into(),
                    chunk: std::mem::take(&mut self.pending),
                },
            )?;
        }
        self.last_flush = Instant::now();
        Ok(())
    }

    fn finish(mut self) -> Result<String, Box<dyn std::error::Error>> {
        if self.truncated {
            self.response.push_str(Self::TRUNCATION_NOTICE);
            self.pending.push_str(Self::TRUNCATION_NOTICE);
        }
        self.flush()?;
        Ok(self.response)
    }
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
    let api_key = provider_api_key("OPENAI_API_KEY")?;
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
    let api_key = provider_api_key("OPENAI_API_KEY")?;
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
    let api_key = provider_api_key("GROQ_API_KEY")?;

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
    fn terminal_shell_field_has_a_concrete_hover_explanation_path() {
        assert!(is_terminal_command_field(
            Some("Terminal"),
            "shell text field",
            Some("AXTextField")
        ));
        assert!(is_terminal_command_field(
            Some("iTerm2"),
            "Input field",
            Some("AXTextField")
        ));
        assert!(!is_terminal_command_field(
            Some("Notes"),
            "Input field",
            Some("AXTextField")
        ));
    }

    #[test]
    fn palette_preserves_questions_but_recognizes_concrete_task_statements() {
        assert!(matches!(
            parse_palette_intent("plan Open Notes and make a checklist"),
            PaletteIntent::Plan(_)
        ));
        assert!(matches!(
            parse_palette_intent("help me understand lifetimes"),
            PaletteIntent::Ask(_)
        ));
        assert!(matches!(
            parse_palette_intent("I want you to go to Canva"),
            PaletteIntent::Ask(_)
        ));
        assert!(looks_like_goal_statement("go to Canva and start a design"));
        assert!(looks_like_goal_statement(
            "create a checklist for my project"
        ));
        assert!(!looks_like_goal_statement("OpenAI pricing"));
        assert!(!looks_like_goal_statement(
            "help me understand Rust lifetimes"
        ));
        // Natural phrasing that mentions "plan" without a leading command
        // verb must still route into the reviewed Goal Guide, not a
        // one-shot Ask whose answer cannot be resumed after Escape.
        assert!(looks_like_goal_statement(
            "I want to design a wedding invitation in Canva. Help me plan that for my brother's wedding."
        ));
        assert!(looks_like_goal_statement(
            "can you help me plan a trip to Japan"
        ));
        assert!(looks_like_goal_statement("I need a plan for onboarding"));
        // "explanation" contains "plan" as a substring but not as a word.
        assert!(!looks_like_goal_statement(
            "give me an explanation of lifetimes"
        ));
    }

    #[test]
    fn local_goal_plans_accept_normal_fenced_json_and_have_a_safe_recovery() {
        let fenced = r#"```json
{"steps":[
 {"instruction":"Open Canva in your browser.","app_hint":"Canva","sensitive":false},
 {"instruction":"Choose a design type that fits your idea.","app_hint":"Canva","sensitive":false},
 {"instruction":"Create the design and review it before sharing.","app_hint":"Canva","sensitive":false}
]}
```"#;
        let plan = parse_goal_plan(fenced).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].app_hint, "Canva");

        let recovery = local_goal_plan_recovery("go to Canva and start a design");
        assert_eq!(recovery.steps.len(), 3);
        assert_eq!(recovery.steps[0].app_hint, "Canva in your browser");
        assert!(validate_goal_plan(&recovery).is_ok());
    }

    #[test]
    fn json_extraction_never_slices_backward_on_model_prose() {
        let malformed = "A stray } appears before the plan: {\"steps\":[]}";
        assert_eq!(extract_json_object(malformed), "{\"steps\":[]}");
        assert_eq!(extract_json_object("no JSON here"), "no JSON here");
    }

    #[test]
    fn palette_truncation_reserves_the_complete_notice() {
        assert_eq!(
            PaletteStream::response_budget() + PaletteStream::TRUNCATION_NOTICE.chars().count(),
            PaletteStream::MAX_RESULT_CHARS
        );
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

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct AxiRecipe {
    pub recipe_id: String,
    pub goal_pattern: String,
    pub goal_embedding: Vec<f32>,
    pub steps: Vec<RecipeStep>,
}

/// A recorded action plus the (role, accessible-name) identity its target
/// resolved to when the step was taught. The uid inside `call` is only
/// valid for the session it was recorded in; replay re-resolves a live uid
/// from `target_role`/`target_context` instead of reusing it directly.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct RecipeStep {
    pub call: tools::ToolCall,
    pub target_role: Option<String>,
    pub target_context: Option<String>,
}

pub fn recipes_dir() -> PathBuf {
    config_path()
        .unwrap_or_else(|| PathBuf::from("."))
        .parent()
        .unwrap()
        .join("recipes")
}

pub fn save_recipe(recipe: &AxiRecipe) -> std::io::Result<()> {
    let dir = recipes_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", recipe.recipe_id));
    let content = serde_json::to_string_pretty(recipe)?;
    std::fs::write(path, content)
}

pub fn load_recipes() -> Vec<AxiRecipe> {
    let mut recipes = Vec::new();
    let dir = recipes_dir();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(recipe) = serde_json::from_str::<AxiRecipe>(&content) {
                        recipes.push(recipe);
                    }
                }
            }
        }
    }
    recipes
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

#[cfg(test)]
mod recipe_matching_tests {
    use super::*;

    #[test]
    fn cosine_similarity_ranks_identical_over_orthogonal_over_opposite() {
        let identical = cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]);
        let orthogonal = cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]);
        let opposite = cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]);
        assert!((identical - 1.0).abs() < 1e-6);
        assert!(orthogonal.abs() < 1e-6);
        assert!((opposite + 1.0).abs() < 1e-6);
        assert!(identical > orthogonal && orthogonal > opposite);
    }

    #[test]
    fn cosine_similarity_treats_a_zero_vector_as_no_match_not_a_panic() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }
}
