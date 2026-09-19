use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mice_core::{
    ConfidenceLevel, IntentRoutingContext, JevRoutingDecision, SidekickEngine, SidekickStatus,
    SidekickTask, SidekickTaskKind, create_default_router,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

/// Closed set of candidate local sidekick tools presented for intent clarification.
pub const CLARIFICATION_CANDIDATE_TOOLS: &[SidekickTaskKind] = &[
    SidekickTaskKind::CodePatch,
    SidekickTaskKind::TestRun,
    SidekickTaskKind::AstSearch,
    SidekickTaskKind::SemanticSearch,
    SidekickTaskKind::FileOpen,
    SidekickTaskKind::FileRead,
    SidekickTaskKind::KnowledgeQuery,
    SidekickTaskKind::General,
];

/// Interactive state for pausing and prompting the user in the Ratatui TUI
/// when Jev intent routing returns Medium or Low confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct ClarificationPromptState {
    pub instruction: String,
    pub suggested_tool: SidekickTaskKind,
    pub confidence: f64,
    pub confidence_level: ConfidenceLevel,
    pub reasoning: String,
    pub selected_idx: usize,
}

impl ClarificationPromptState {
    pub fn from_decision(instruction: impl Into<String>, decision: &JevRoutingDecision) -> Self {
        let suggested_idx = CLARIFICATION_CANDIDATE_TOOLS
            .iter()
            .position(|&t| t == decision.tool)
            .unwrap_or(0);

        Self {
            instruction: instruction.into(),
            suggested_tool: decision.tool,
            confidence: decision.confidence,
            confidence_level: decision.confidence_level,
            reasoning: decision.reasoning.clone(),
            selected_idx: suggested_idx,
        }
    }

    pub fn selected_tool(&self) -> SidekickTaskKind {
        CLARIFICATION_CANDIDATE_TOOLS
            .get(self.selected_idx)
            .copied()
            .unwrap_or(self.suggested_tool)
    }

    pub fn next(&mut self) {
        if self.selected_idx + 1 < CLARIFICATION_CANDIDATE_TOOLS.len() {
            self.selected_idx += 1;
        } else {
            self.selected_idx = 0;
        }
    }

    pub fn prev(&mut self) {
        if self.selected_idx > 0 {
            self.selected_idx -= 1;
        } else {
            self.selected_idx = CLARIFICATION_CANDIDATE_TOOLS.len().saturating_sub(1);
        }
    }
}

/// Create a concrete SidekickTask configured for the given tool kind and prompt.
pub fn task_for_tool(kind: SidekickTaskKind, prompt: &str, task_num: usize) -> SidekickTask {
    match kind {
        SidekickTaskKind::CodePatch => SidekickTask {
            id: format!("task_patch_{task_num}"),
            kind: SidekickTaskKind::CodePatch,
            prompt: prompt.into(),
            working_dir: PathBuf::from("crates/mice-core/src"),
            target_files: vec!["sidekick.rs".into()],
            code_patch_target: Some("sidekick.rs".into()),
            old_content: Some("// Sub-millisecond semantic document & file retrieval".into()),
            new_content: Some("/// Sub-millisecond semantic document & file retrieval".into()),
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
        SidekickTaskKind::TestRun => SidekickTask {
            id: format!("task_test_{task_num}"),
            kind: SidekickTaskKind::TestRun,
            prompt: prompt.into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: Some("cargo test -p mice-core --lib jev".into()),
            orchestrator: "Jev System One".into(),
        },
        SidekickTaskKind::AstSearch => SidekickTask {
            id: format!("task_ast_{task_num}"),
            kind: SidekickTaskKind::AstSearch,
            prompt: if prompt.contains("Sidekick") {
                "SidekickEngine".into()
            } else {
                prompt.into()
            },
            working_dir: PathBuf::from("crates/mice-core/src"),
            target_files: vec!["sidekick.rs".into(), "lib.rs".into()],
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
        SidekickTaskKind::SemanticSearch => SidekickTask {
            id: format!("task_sem_{task_num}"),
            kind: SidekickTaskKind::SemanticSearch,
            prompt: prompt.into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
        SidekickTaskKind::FileOpen => SidekickTask {
            id: format!("task_open_{task_num}"),
            kind: SidekickTaskKind::FileOpen,
            prompt: prompt.into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
        SidekickTaskKind::FileRead => SidekickTask {
            id: format!("task_read_{task_num}"),
            kind: SidekickTaskKind::FileRead,
            prompt: prompt.into(),
            working_dir: PathBuf::from("."),
            target_files: vec!["Cargo.toml".into()],
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
        SidekickTaskKind::KnowledgeQuery => SidekickTask {
            id: format!("task_kg_{task_num}"),
            kind: SidekickTaskKind::KnowledgeQuery,
            prompt: prompt.into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
        _ => SidekickTask {
            id: format!("task_gen_{task_num}"),
            kind: SidekickTaskKind::General,
            prompt: prompt.into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Jev System One".into(),
        },
    }
}

/// Run the MICE Sidekick Interactive TUI dashboard.
pub fn run_sidekick_tui() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut engine = SidekickEngine::new();
    let mut selected_task_idx: usize = 0;
    let mut notification: Option<(String, Instant)> = None;
    let router = create_default_router(None);
    let mut clarification_prompt: Option<ClarificationPromptState> = None;
    let mut route_sample_idx: usize = 0;

    // Seed initial demo/welcome tasks
    let welcome_task = SidekickTask {
        id: "task_init_1".into(),
        kind: SidekickTaskKind::SemanticSearch,
        prompt: "Index identity and financial documents in local corpus".into(),
        working_dir: PathBuf::from("."),
        target_files: Vec::new(),
        code_patch_target: None,
        old_content: None,
        new_content: None,
        test_command: None,
        orchestrator: "Gemini 3.7 Flash".into(),
    };
    let _ = engine.execute(&welcome_task);

    let kg_task = SidekickTask {
        id: "task_init_2".into(),
        kind: SidekickTaskKind::KnowledgeQuery,
        prompt: "Traverse connected entities for UIDAI and Aadhaar".into(),
        working_dir: PathBuf::from("."),
        target_files: Vec::new(),
        code_patch_target: None,
        old_content: None,
        new_content: None,
        test_command: None,
        orchestrator: "Claude 3.7 Sonnet".into(),
    };
    let _ = engine.execute(&kg_task);

    use std::time::Instant;

    'app: loop {
        terminal.draw(|f| {
            render_sidekick_dashboard(
                f,
                &engine,
                selected_task_idx,
                notification.as_ref(),
                clarification_prompt.as_ref(),
            );
        })?;

        if event::poll(Duration::from_millis(30))? {
            let mut current_ev = Some(event::read()?);
            while let Some(ev) = current_ev {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(ref mut prompt) = clarification_prompt {
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => {
                                    clarification_prompt = None;
                                    notification =
                                        Some(("Clarification cancelled.".into(), Instant::now()));
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    prompt.prev();
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    prompt.next();
                                }
                                KeyCode::Char(c) if ('1'..='8').contains(&c) => {
                                    let idx = (c as usize) - ('1' as usize);
                                    if idx < CLARIFICATION_CANDIDATE_TOOLS.len() {
                                        prompt.selected_idx = idx;
                                    }
                                }
                                KeyCode::Enter => {
                                    let chosen_tool = prompt.selected_tool();
                                    let new_task = task_for_tool(
                                        chosen_tool,
                                        &prompt.instruction,
                                        engine.task_history.len() + 1,
                                    );
                                    if let Ok(res) = engine.execute(&new_task) {
                                        selected_task_idx =
                                            engine.task_history.len().saturating_sub(1);
                                        notification = Some((
                                            format!(
                                                "Clarified & Executed {}: saved {} tokens!",
                                                chosen_tool.display_label(),
                                                res.token_savings.net_tokens_saved
                                            ),
                                            Instant::now(),
                                        ));
                                    }
                                    clarification_prompt = None;
                                }
                                _ => {}
                            }
                        } else {
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => break 'app,
                                KeyCode::Char('r') | KeyCode::Char('i') => {
                                    let sample_commands = [
                                        "fix the lint error in lib.rs",
                                        "run tests for mice-core",
                                        "where is SidekickEngine defined",
                                        "locate my electricity bill invoice",
                                        "open 12th report card in system viewer",
                                    ];
                                    let cmd =
                                        sample_commands[route_sample_idx % sample_commands.len()];
                                    route_sample_idx += 1;

                                    let ctx = IntentRoutingContext::current();
                                    let decision = router.route(cmd, &ctx);

                                    if decision.confidence_level.should_auto_run() {
                                        let new_task = task_for_tool(
                                            decision.tool,
                                            cmd,
                                            engine.task_history.len() + 1,
                                        );
                                        if let Ok(res) = engine.execute(&new_task) {
                                            selected_task_idx =
                                                engine.task_history.len().saturating_sub(1);
                                            notification = Some((
                                                format!(
                                                    "⚡ Jev Auto-run (High {:.0}%): Executed {} (+{} tok)",
                                                    decision.confidence * 100.0,
                                                    decision.tool.display_label(),
                                                    res.token_savings.net_tokens_saved
                                                ),
                                                Instant::now(),
                                            ));
                                        }
                                    } else {
                                        clarification_prompt = Some(
                                            ClarificationPromptState::from_decision(cmd, &decision),
                                        );
                                    }
                                }
                                KeyCode::Char('v') => {
                                    let cmd = "help me think about stuff";
                                    let ctx = IntentRoutingContext::current();
                                    let decision = router.route(cmd, &ctx);
                                    clarification_prompt = Some(
                                        ClarificationPromptState::from_decision(cmd, &decision),
                                    );
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    selected_task_idx = navigate_selection(
                                        selected_task_idx,
                                        -1,
                                        engine.task_history.len(),
                                    );
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    selected_task_idx = navigate_selection(
                                        selected_task_idx,
                                        1,
                                        engine.task_history.len(),
                                    );
                                }
                                KeyCode::Home => {
                                    selected_task_idx = 0;
                                }
                                KeyCode::End => {
                                    selected_task_idx = if engine.task_history.is_empty() {
                                        0
                                    } else {
                                        engine.task_history.len().saturating_sub(1)
                                    };
                                }
                                KeyCode::PageUp => {
                                    selected_task_idx = navigate_selection(
                                        selected_task_idx,
                                        -5,
                                        engine.task_history.len(),
                                    );
                                }
                                KeyCode::PageDown => {
                                    selected_task_idx = navigate_selection(
                                        selected_task_idx,
                                        5,
                                        engine.task_history.len(),
                                    );
                                }
                                KeyCode::Char('d') => {
                                    // Delegate sample AST search task
                                    let new_task = SidekickTask {
                                        id: format!("task_ast_{}", engine.task_history.len() + 1),
                                        kind: SidekickTaskKind::AstSearch,
                                        prompt: "SidekickEngine".into(),
                                        working_dir: PathBuf::from("crates/mice-core/src"),
                                        target_files: vec!["sidekick.rs".into(), "lib.rs".into()],
                                        code_patch_target: None,
                                        old_content: None,
                                        new_content: None,
                                        test_command: None,
                                        orchestrator: "Gemini 3.7 Flash".into(),
                                    };
                                    if let Ok(res) = engine.execute(&new_task) {
                                        selected_task_idx =
                                            engine.task_history.len().saturating_sub(1);
                                        notification = Some((
                                            format!(
                                                "Delegated AST search: saved {} tokens!",
                                                res.token_savings.net_tokens_saved
                                            ),
                                            Instant::now(),
                                        ));
                                    }
                                }
                                KeyCode::Char('s') => {
                                    // Run semantic search
                                    let s_task = SidekickTask {
                                        id: format!("task_sem_{}", engine.task_history.len() + 1),
                                        kind: SidekickTaskKind::SemanticSearch,
                                        prompt: "electricity bill utility".into(),
                                        working_dir: PathBuf::from("."),
                                        target_files: Vec::new(),
                                        code_patch_target: None,
                                        old_content: None,
                                        new_content: None,
                                        test_command: None,
                                        orchestrator: "Antigravity".into(),
                                    };
                                    if let Ok(res) = engine.execute(&s_task) {
                                        selected_task_idx =
                                            engine.task_history.len().saturating_sub(1);
                                        notification = Some((
                                            format!(
                                                "Semantic Finder resolved query: saved {} tokens",
                                                res.token_savings.net_tokens_saved
                                            ),
                                            Instant::now(),
                                        ));
                                    }
                                }
                                KeyCode::Char('t') => {
                                    // Run local test
                                    let t_task = SidekickTask {
                                        id: format!("task_test_{}", engine.task_history.len() + 1),
                                        kind: SidekickTaskKind::TestRun,
                                        prompt: "Run fast test suite".into(),
                                        working_dir: PathBuf::from("."),
                                        target_files: Vec::new(),
                                        code_patch_target: None,
                                        old_content: None,
                                        new_content: None,
                                        test_command: Some("cargo test -p mice-core --lib".into()),
                                        orchestrator: "Gemini 3.7 Flash".into(),
                                    };
                                    if let Ok(res) = engine.execute(&t_task) {
                                        selected_task_idx =
                                            engine.task_history.len().saturating_sub(1);
                                        notification = Some((
                                            format!("Test run finished: status {:?}", res.status),
                                            Instant::now(),
                                        ));
                                    }
                                }
                                KeyCode::Char('p') => {
                                    // Apply sample code patch
                                    let p_task = SidekickTask {
                                    id: format!("task_patch_{}", engine.task_history.len() + 1),
                                    kind: SidekickTaskKind::CodePatch,
                                    prompt: "Verified code patch preview".into(),
                                    working_dir: PathBuf::from("crates/mice-core/src"),
                                    target_files: vec!["sidekick.rs".into()],
                                    code_patch_target: Some("sidekick.rs".into()),
                                    old_content: Some(
                                        "// Sub-millisecond semantic document & file retrieval"
                                            .into(),
                                    ),
                                    new_content: Some(
                                        "/// Sub-millisecond semantic document & file retrieval"
                                            .into(),
                                    ),
                                    test_command: None,
                                    orchestrator: "Claude 3.7 Sonnet".into(),
                                };
                                    if let Ok(res) = engine.execute(&p_task) {
                                        selected_task_idx =
                                            engine.task_history.len().saturating_sub(1);
                                        notification = Some((
                                            format!(
                                                "Patch verified and applied! (+{} tokens saved)",
                                                res.token_savings.net_tokens_saved
                                            ),
                                            Instant::now(),
                                        ));
                                    }
                                }
                                KeyCode::Char('c') => {
                                    engine.task_history.clear();
                                    selected_task_idx = 0;
                                    notification =
                                        Some(("Task history cleared.".into(), Instant::now()));
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Resize(_, _) => {
                        terminal.autoresize()?;
                    }
                    _ => {}
                }

                if event::poll(Duration::from_millis(0))? {
                    current_ev = Some(event::read()?);
                } else {
                    current_ev = None;
                }
            }
        }

        // Auto expire notifications after 4 seconds
        if let Some((_, time)) = &notification
            && time.elapsed() > Duration::from_secs(4)
        {
            notification = None;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Clamp task selection index safely to [0, count - 1].
pub fn clamp_selected_idx(selected_idx: usize, task_count: usize) -> usize {
    if task_count == 0 {
        0
    } else {
        selected_idx.min(task_count - 1)
    }
}

/// Navigate task selection index by a relative delta with upper/lower bounds checking.
pub fn navigate_selection(current: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let max_idx = count - 1;
    if delta < 0 {
        current.saturating_sub((-delta) as usize)
    } else {
        (current + delta as usize).min(max_idx)
    }
}

/// Render the interactive Sidekick dashboard onto a Ratatui frame.
pub fn render_sidekick_dashboard(
    f: &mut ratatui::Frame,
    engine: &SidekickEngine,
    selected_idx: usize,
    notification: Option<&(String, std::time::Instant)>,
    clarification_prompt: Option<&ClarificationPromptState>,
) {
    let size = f.area();
    let effective_idx = clamp_selected_idx(selected_idx, engine.task_history.len());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header & Paired status
            Constraint::Length(4), // Metrics banner
            Constraint::Min(10),   // Main content (tasks + details)
            Constraint::Length(3), // Keybindings footer
        ])
        .split(size);

    // 1. Header
    let header_text = vec![
        Line::from(vec![
            Span::styled(
                "🐭 MICE ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "• Terminal Sidekick Sub-Agent for Coding LLMs",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[⚡ Paired: Gemini 3.7 Flash + Claude + Antigravity]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![Span::styled(
            "Devin SWE 2.0 Sidekick Architecture - Offload token-heavy routines, return verified diffs",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let header_p = Paragraph::new(header_text).block(header_block);
    f.render_widget(header_p, chunks[0]);

    // 2. Metrics Banner
    let total_tasks = engine.task_history.len();
    let net_tokens = engine.cumulative_tokens_saved;
    let cost_saved = engine.cumulative_cost_saved_usd;
    let avg_ratio = if total_tasks > 0 {
        let sum_ratio: f64 = engine
            .task_history
            .iter()
            .map(|t| t.token_savings.compression_ratio)
            .sum();
        sum_ratio / (total_tasks as f64)
    } else {
        1.0
    };

    let metric_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(chunks[1]);

    let m1 = Paragraph::new(vec![
        Line::from(Span::styled(
            "TASKS DELEGATED",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{} tasks", total_tasks),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    let m2 = Paragraph::new(vec![
        Line::from(Span::styled(
            "NET TOKENS OFFLOADED",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{} tokens", format_number(net_tokens)),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    let m3 = Paragraph::new(vec![
        Line::from(Span::styled(
            "EST. ORCHESTRATOR SAVINGS",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("${:.4} USD", cost_saved),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    let m4 = Paragraph::new(vec![
        Line::from(Span::styled(
            "COMPRESSION RATIO",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{:.1}x context efficiency", avg_ratio),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(m1, metric_chunks[0]);
    f.render_widget(m2, metric_chunks[1]);
    f.render_widget(m3, metric_chunks[2]);
    f.render_widget(m4, metric_chunks[3]);

    // 3. Main Body (Tasks list on left, Details/Diff on right)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[2]);

    // Left: Task List
    let items: Vec<ListItem> = if engine.task_history.is_empty() {
        vec![ListItem::new(vec![
            Line::from(Span::styled(
                "  No active tasks in history",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  Press [d] or [s] to delegate",
                Style::default().fg(Color::DarkGray),
            )),
        ])]
    } else {
        engine
            .task_history
            .iter()
            .enumerate()
            .map(|(i, task)| {
                let selected = i == effective_idx;
                let status_style = match task.status {
                    SidekickStatus::Success => Style::default().fg(Color::Green),
                    SidekickStatus::Failed => Style::default().fg(Color::Red),
                    SidekickStatus::Running => Style::default().fg(Color::Yellow),
                    _ => Style::default().fg(Color::DarkGray),
                };

                let prefix = if selected { "▶ " } else { "  " };
                let line1 = Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{} ", task.task_id),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("[{:?}] ", task.status), status_style),
                    Span::styled(
                        format!(
                            "+{} tok",
                            format_number(task.token_savings.net_tokens_saved)
                        ),
                        Style::default().fg(Color::Green),
                    ),
                ]);

                let line2 = Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        if task.summary.len() > 45 {
                            format!("{}...", &task.summary[..45])
                        } else {
                            task.summary.clone()
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);

                let item_style = if selected {
                    Style::default().bg(Color::Rgb(20, 30, 45))
                } else {
                    Style::default()
                };

                ListItem::new(vec![line1, line2]).style(item_style)
            })
            .collect()
    };

    let task_list = List::new(items).block(
        Block::default()
            .title(" Delegated Sub-Agent Tasks (↑/↓ to select) ")
            .title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(task_list, body_chunks[0]);

    // Right: Selected Task Details & Diff View
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body_chunks[1]);

    let selected_task_opt = if engine.task_history.is_empty() {
        None
    } else {
        engine.task_history.get(effective_idx)
    };

    if let Some(selected_task) = selected_task_opt {
        // Top right: Task summary & token metrics
        let mut detail_lines = vec![
            Line::from(vec![
                Span::styled(
                    "Task ID: ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(&selected_task.task_id, Style::default().fg(Color::White)),
                Span::raw("  |  Status: "),
                Span::styled(
                    format!("{:?}", selected_task.status),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  |  Duration: "),
                Span::styled(
                    format!("{} ms", selected_task.duration_ms),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "Summary: ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(&selected_task.summary),
            ]),
            Line::from(vec![
                Span::styled("Offloaded Tokens: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{} input + {} output",
                        selected_task.token_savings.input_tokens_offloaded,
                        selected_task.token_savings.output_tokens_offloaded
                    ),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(" → Returned: "),
                Span::styled(
                    format!(
                        "{} tokens",
                        selected_task.token_savings.tokens_returned_to_orchestrator
                    ),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
        ];

        if !selected_task.matched_items.is_empty() {
            detail_lines.push(Line::from(Span::styled(
                "Matched Items:",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            for m in selected_task.matched_items.iter().take(4) {
                detail_lines.push(Line::from(vec![
                    Span::raw("  • "),
                    Span::styled(m, Style::default().fg(Color::White)),
                ]));
            }
        }

        let detail_p = Paragraph::new(detail_lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Sub-Agent Task Details ")
                    .title_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
        f.render_widget(detail_p, right_chunks[0]);

        // Bottom right: Diff Preview or Test Output
        let diff_lines = if let Some(diff) = &selected_task.diff_preview {
            parse_diff_lines(diff)
        } else if let Some(test_out) = &selected_task.test_output {
            let mut lines = Vec::new();
            lines.push(Line::from(Span::styled(
                "--- Test Output Stream ---",
                Style::default().fg(Color::DarkGray),
            )));
            for line in test_out.lines().take(12) {
                let style = if line.contains("FAILED") || line.contains("error") {
                    Style::default().fg(Color::Red)
                } else if line.contains("ok") || line.contains("passed") {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
            lines
        } else {
            vec![
                Line::from(Span::styled(
                    "No file diff generated for this task type.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "MICE executed this sub-routine locally and returned verified context.",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        };

        let diff_p = Paragraph::new(diff_lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Diff & Artifact Inspector ")
                .title_style(
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(diff_p, right_chunks[1]);
    } else {
        let empty_details = Paragraph::new(vec![
            Line::from(Span::styled(
                "No delegated sub-agent tasks recorded yet.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "[d] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("Delegate AST symbol search across codebase"),
            ]),
            Line::from(vec![
                Span::styled(
                    "[s] ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("Run semantic search in local documents"),
            ]),
            Line::from(vec![
                Span::styled(
                    "[t] ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("Execute fast local test suite"),
            ]),
            Line::from(vec![
                Span::styled(
                    "[p] ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("Preview verified surgical code patch"),
            ]),
        ])
        .block(
            Block::default()
                .title(" Sub-Agent Task Details ")
                .title_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(empty_details, right_chunks[0]);

        let empty_diff = Paragraph::new(vec![
            Line::from(Span::styled(
                "No diff or artifacts to inspect.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Delegate a code patch or sub-agent routine to preview unified diffs and token savings.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(
            Block::default()
                .title(" Diff & Artifact Inspector ")
                .title_style(
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(empty_diff, right_chunks[1]);
    }

    // 4. Footer & Keybindings
    let mut footer_spans = if size.width >= 120 {
        vec![
            Span::styled(
                "[r] ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Route Intent  "),
            Span::styled(
                "[v] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Vague Test  "),
            Span::styled(
                "[d] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("AST  "),
            Span::styled(
                "[s] ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Semantic  "),
            Span::styled(
                "[t] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Run Tests  "),
            Span::styled(
                "[p] ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Apply Patch  "),
            Span::styled("[c] ", Style::default().fg(Color::Red)),
            Span::raw("Clear  "),
            Span::styled(
                "[q] ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Quit"),
        ]
    } else {
        vec![
            Span::styled(
                "[r] ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Route "),
            Span::styled(
                "[v] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Vague "),
            Span::styled(
                "[d] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("AST "),
            Span::styled(
                "[s] ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Search "),
            Span::styled(
                "[t] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Test "),
            Span::styled(
                "[p] ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Patch "),
            Span::styled("[c] ", Style::default().fg(Color::Red)),
            Span::raw("Clr "),
            Span::styled(
                "[q] ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Quit"),
        ]
    };

    if let Some((notif, _)) = notification {
        footer_spans.push(Span::raw("  |  "));
        footer_spans.push(Span::styled(
            notif,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let footer_p = Paragraph::new(Line::from(footer_spans))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan)),
        );
    f.render_widget(footer_p, chunks[3]);

    if let Some(prompt) = clarification_prompt {
        render_clarification_dialog(f, prompt);
    }
}

/// Render an interactive modal dialog when Jev intent routing pauses for clarification.
pub fn render_clarification_dialog(f: &mut ratatui::Frame, prompt: &ClarificationPromptState) {
    let area = f.area();
    let popup_width = 74.min(area.width.saturating_sub(4));
    let popup_height = 18.min(area.height.saturating_sub(2));

    let vertical_pad = (area.height.saturating_sub(popup_height)) / 2;
    let horizontal_pad = (area.width.saturating_sub(popup_width)) / 2;

    let popup_area = Rect {
        x: area.x + horizontal_pad,
        y: area.y + vertical_pad,
        width: popup_width,
        height: popup_height,
    };

    f.render_widget(Clear, popup_area);

    let border_color = match prompt.confidence_level {
        ConfidenceLevel::High => Color::Green,
        ConfidenceLevel::Medium => Color::Yellow,
        ConfidenceLevel::Low => Color::Red,
    };

    let block = Block::default()
        .title(" 🐭 JEV INTENT ROUTING - CLARIFICATION PAUSED ")
        .title_style(
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let inner_area = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let dialog_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Instruction & confidence banner
            Constraint::Min(8),    // Candidate tools list
            Constraint::Length(2), // Footer buttons
        ])
        .split(inner_area);

    let banner = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "Command: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\"{}\"", prompt.instruction),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Suggested: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} ({})",
                    prompt.suggested_tool.display_label(),
                    prompt.suggested_tool.tool_name()
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  |  Confidence: "),
            Span::styled(
                format!(
                    "{:.1}% ({})",
                    prompt.confidence * 100.0,
                    prompt.confidence_level.label()
                ),
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "Confidence is below 80% auto-run threshold. Please confirm or pick the intended tool:",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::NONE));
    f.render_widget(banner, dialog_chunks[0]);

    let tool_items: Vec<ListItem> = CLARIFICATION_CANDIDATE_TOOLS
        .iter()
        .enumerate()
        .map(|(idx, &tool)| {
            let is_selected = idx == prompt.selected_idx;
            let is_suggested = tool == prompt.suggested_tool;
            let prefix = if is_selected { "▶ " } else { "  " };
            let num = idx + 1;

            let mut spans = vec![
                Span::styled(
                    prefix,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("[{num}] "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{} ", tool.emoji())),
                Span::styled(
                    format!("{:<18}", tool.display_label()),
                    if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(
                    format!("({})", tool.tool_name()),
                    Style::default().fg(Color::DarkGray),
                ),
            ];

            if is_suggested {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    "[Jev Suggested]",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            let style = if is_selected {
                Style::default().bg(Color::Rgb(30, 40, 60))
            } else {
                Style::default()
            };

            ListItem::new(Line::from(spans)).style(style)
        })
        .collect();

    let list = List::new(tool_items).block(
        Block::default()
            .title(" Candidate Tools ")
            .title_style(Style::default().fg(Color::DarkGray))
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(list, dialog_chunks[1]);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            "[↑/↓ or 1-8] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Select  |  "),
        Span::styled(
            "[Enter] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Confirm & Run  |  "),
        Span::styled(
            "[Esc] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("Cancel"),
    ]))
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(footer, dialog_chunks[2]);
}

fn format_number(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Execute a delegated task via CLI arguments and print output or JSON.
pub fn execute_delegate_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let mut explicit_kind = None;
    let mut prompt = String::new();
    let mut target_files = Vec::new();
    let mut test_cmd = None;
    let mut orchestrator = "Gemini 3.7 Flash".to_string();
    let mut as_json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" | "-k" => {
                if i + 1 < args.len() {
                    explicit_kind = Some(match args[i + 1].to_lowercase().as_str() {
                        "semantic" | "search" => SidekickTaskKind::SemanticSearch,
                        "open" | "file_open" | "launch" => SidekickTaskKind::FileOpen,
                        "ast" | "symbol" => SidekickTaskKind::AstSearch,
                        "patch" | "code_patch" => SidekickTaskKind::CodePatch,
                        "test" | "tests" => SidekickTaskKind::TestRun,
                        "read" | "file" => SidekickTaskKind::FileRead,
                        "kg" | "knowledge" => SidekickTaskKind::KnowledgeQuery,
                        "batch" => SidekickTaskKind::BatchEdit,
                        _ => SidekickTaskKind::General,
                    });
                    i += 1;
                }
            }
            "--open" | "-O" => {
                explicit_kind = Some(SidekickTaskKind::FileOpen);
            }
            "--file" | "-f" => {
                if i + 1 < args.len() {
                    target_files.push(args[i + 1].clone());
                    i += 1;
                }
            }
            "--test-cmd" | "-t" => {
                if i + 1 < args.len() {
                    test_cmd = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--orchestrator" | "-o" => {
                if i + 1 < args.len() {
                    orchestrator = args[i + 1].clone();
                    i += 1;
                }
            }
            "--json" => {
                as_json = true;
            }
            arg if !arg.starts_with('-') && prompt.is_empty() => {
                prompt = arg.to_string();
            }
            _ => {}
        }
        i += 1;
    }

    if prompt.is_empty() {
        prompt = "Execute delegated sub-agent routine".into();
    }

    // Determine task kind: either explicit or Jev intent routed
    let (mut kind, routing_decision) = if let Some(k) = explicit_kind {
        (k, None)
    } else {
        let router = create_default_router(None);
        let ctx = IntentRoutingContext::current();
        let dec = router.route(&prompt, &ctx);
        (dec.tool, Some(dec))
    };

    if let Some(ref dec) = routing_decision {
        if dec.confidence_level.should_auto_run() {
            if !as_json {
                println!(
                    "⚡ Jev High Confidence ({:.1}%): Auto-running '{}' ({})",
                    dec.confidence * 100.0,
                    kind.tool_name(),
                    kind.display_label()
                );
            }
        } else {
            // Medium/Low confidence: pause and prompt in terminal if interactive
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() && !as_json {
                println!(
                    "\n🐭 Jev Intent Routing: Clarification Required\n  Instruction: \"{}\"\n  Suggested:   {} ({}) [Confidence: {:.1}%, {}]\n  Reasoning:   {}\n",
                    prompt,
                    kind.display_label(),
                    kind.tool_name(),
                    dec.confidence * 100.0,
                    dec.confidence_level.label(),
                    dec.reasoning
                );
                println!("Available tools:");
                for (idx, candidate) in CLARIFICATION_CANDIDATE_TOOLS.iter().enumerate() {
                    let mark = if *candidate == kind {
                        " [Jev Suggested]"
                    } else {
                        ""
                    };
                    println!(
                        "  [{}] {} ({}){}",
                        idx + 1,
                        candidate.display_label(),
                        candidate.tool_name(),
                        mark
                    );
                }
                print!(
                    "\nConfirm [{}]? (Press Enter to run, 1-8 to switch, or 'q' to cancel): ",
                    kind.tool_name()
                );
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let trimmed = line.trim();
                if trimmed.eq_ignore_ascii_case("q") || trimmed.eq_ignore_ascii_case("cancel") {
                    println!("Execution cancelled.");
                    return Ok(());
                } else if let Ok(num) = trimmed.parse::<usize>()
                    && (1..=CLARIFICATION_CANDIDATE_TOOLS.len()).contains(&num)
                {
                    kind = CLARIFICATION_CANDIDATE_TOOLS[num - 1];
                    println!("Switched to: {}", kind.display_label());
                }
            } else if !as_json {
                println!(
                    "ℹ Jev Routed: '{}' ({:.1}% confidence, {})",
                    kind.display_label(),
                    dec.confidence * 100.0,
                    dec.confidence_level.label()
                );
            }
        }
    }

    if target_files.is_empty() {
        for word in prompt.split_whitespace() {
            let clean = word.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-'
            });
            if clean.ends_with(".rs")
                || clean.ends_with(".toml")
                || clean.ends_with(".json")
                || clean.ends_with(".md")
            {
                target_files.push(clean.to_string());
                break;
            }
        }
    }

    let task = SidekickTask {
        id: format!("task_cli_{}", std::process::id()),
        kind,
        prompt: prompt.clone(),
        working_dir: std::env::current_dir()?,
        target_files,
        code_patch_target: None,
        old_content: None,
        new_content: None,
        test_command: test_cmd,
        orchestrator,
    };

    let mut engine = SidekickEngine::new();
    let result = engine.execute(&task)?;

    if as_json {
        let mut json_val = serde_json::to_value(&result)?;
        if let Some(dec) = routing_decision
            && let Some(map) = json_val.as_object_mut()
        {
            map.insert(
                "jev_routing".to_string(),
                serde_json::json!({
                    "tool": dec.tool.tool_name(),
                    "confidence": dec.confidence,
                    "confidence_level": dec.confidence_level,
                    "should_auto_run": dec.confidence_level.should_auto_run(),
                    "reasoning": dec.reasoning,
                    "is_fallback": dec.is_fallback,
                }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&json_val)?);
    } else {
        println!("🐭 MICE Sidekick Sub-Agent Result");
        println!("Task ID:     {}", result.task_id);
        println!("Status:      {:?}", result.status);
        println!("Duration:    {} ms", result.duration_ms);
        println!("Summary:     {}", result.summary);
        println!("\n⚡ Token Savings Metrics:");
        println!(
            "  - Ingested/Offloaded Tokens: {}",
            format_number(result.token_savings.input_tokens_offloaded)
        );
        println!(
            "  - Returned to Orchestrator:  {} tokens",
            format_number(result.token_savings.tokens_returned_to_orchestrator)
        );
        println!(
            "  - Net Tokens Saved:          {} tokens",
            format_number(result.token_savings.net_tokens_saved)
        );
        println!(
            "  - Estimated Cost Saved:      ${:.4} USD",
            result.token_savings.estimated_cost_saved_usd
        );
        println!(
            "  - Context Compression:       {:.1}x",
            result.token_savings.compression_ratio
        );

        if let Some(diff) = &result.diff_preview {
            println!("\n--- Code Patch Diff ---\n{}", diff);
        }
    }

    Ok(())
}

/// Parse unified diff text into styled Ratatui Lines with syntax highlighting:
/// - Headers (`+++`, `---`, `diff `, `index `): Yellow Bold
/// - Additions (`+`): Green
/// - Deletions (`-`): Red
/// - Hunk markers (`@@`): Cyan Bold
/// - Context lines: DarkGray
pub fn parse_diff_lines(diff: &str) -> Vec<Line<'static>> {
    let mut diff_lines = Vec::new();
    diff_lines.push(Line::from(Span::styled(
        "--- Structured Code Patch Diff ---",
        Style::default().fg(Color::DarkGray),
    )));
    for line in diff.lines() {
        if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            diff_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if line.starts_with('+') {
            diff_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Green),
            )));
        } else if line.starts_with('-') {
            diff_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Red),
            )));
        } else if line.starts_with("@@") {
            diff_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            diff_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    diff_lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_ratatui_layout_splits_standard_80x24() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut engine = SidekickEngine::new();
        // Add sample task
        let task = SidekickTask {
            id: "test_task_1".into(),
            kind: SidekickTaskKind::SemanticSearch,
            prompt: "Aadhaar Card".into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Gemini 3.7 Flash".into(),
        };
        let _ = engine.execute(&task);

        terminal
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 0, None, None);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);

        // Check header title rendered
        let content_str: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content_str.contains("MICE"));
        assert!(content_str.contains("Terminal Sidekick"));
        assert!(content_str.contains("Quit"));
    }

    #[test]
    fn test_ratatui_layout_splits_wide_viewport_160x40() {
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut engine = SidekickEngine::new();
        let task = SidekickTask {
            id: "wide_task_1".into(),
            kind: SidekickTaskKind::SemanticSearch,
            prompt: "Swiggy invoice".into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Claude 3.7 Sonnet".into(),
        };
        let _ = engine.execute(&task);

        terminal
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 0, None, None);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 160);
        assert_eq!(buffer.area.height, 40);
    }

    #[test]
    fn test_task_selection_scroll_bounds_on_empty_history() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let engine = SidekickEngine::new();
        assert_eq!(engine.task_history.len(), 0);

        // Clamping and navigation tests on empty history
        assert_eq!(clamp_selected_idx(0, 0), 0);
        assert_eq!(clamp_selected_idx(10, 0), 0);
        assert_eq!(navigate_selection(0, -1, 0), 0);
        assert_eq!(navigate_selection(0, 1, 0), 0);

        // Draw with arbitrary out-of-bounds selected_idx to verify no panic
        terminal
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 0, None, None);
            })
            .unwrap();

        terminal
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 999, None, None);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content_str: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content_str.contains("No active tasks in history"));
        assert!(content_str.contains("No delegated sub-agent tasks recorded yet."));
    }

    #[test]
    fn test_task_selection_scroll_bounds_on_single_item_history() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut engine = SidekickEngine::new();
        let task = SidekickTask {
            id: "single_task_1".into(),
            kind: SidekickTaskKind::SemanticSearch,
            prompt: "PAN Card".into(),
            working_dir: PathBuf::from("."),
            target_files: Vec::new(),
            code_patch_target: None,
            old_content: None,
            new_content: None,
            test_command: None,
            orchestrator: "Antigravity".into(),
        };
        let _ = engine.execute(&task);
        assert_eq!(engine.task_history.len(), 1);

        // Clamping and navigation tests on single item history
        assert_eq!(clamp_selected_idx(0, 1), 0);
        assert_eq!(clamp_selected_idx(5, 1), 0);
        assert_eq!(navigate_selection(0, -1, 1), 0);
        assert_eq!(navigate_selection(0, 1, 1), 0);

        terminal
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 0, None, None);
            })
            .unwrap();

        terminal
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 10, None, None);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content_str: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content_str.contains("single_task_1"));
    }

    #[test]
    fn test_syntax_highlighted_diff_viewer() {
        let diff_sample = "--- a/src/sample.rs\n+++ b/src/sample.rs\n@@ -1,3 +1,3 @@\n-old_line();\n+new_line();\n context_line();";
        let lines = parse_diff_lines(diff_sample);

        // Line 0: Header banner "--- Structured Code Patch Diff ---"
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));

        // Line 1: "--- a/src/sample.rs" -> Header (Yellow + Bold)
        assert_eq!(lines[1].spans[0].content, "--- a/src/sample.rs");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Yellow));
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );

        // Line 2: "+++ b/src/sample.rs" -> Header (Yellow + Bold)
        assert_eq!(lines[2].spans[0].content, "+++ b/src/sample.rs");
        assert_eq!(lines[2].spans[0].style.fg, Some(Color::Yellow));
        assert!(
            lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );

        // Line 3: "@@ -1,3 +1,3 @@" -> Hunk header (Cyan + Bold)
        assert_eq!(lines[3].spans[0].content, "@@ -1,3 +1,3 @@");
        assert_eq!(lines[3].spans[0].style.fg, Some(Color::Cyan));
        assert!(
            lines[3].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );

        // Line 4: "-old_line();" -> Deletion (Red)
        assert_eq!(lines[4].spans[0].content, "-old_line();");
        assert_eq!(lines[4].spans[0].style.fg, Some(Color::Red));

        // Line 5: "+new_line();" -> Addition (Green)
        assert_eq!(lines[5].spans[0].content, "+new_line();");
        assert_eq!(lines[5].spans[0].style.fg, Some(Color::Green));

        // Line 6: " context_line();" -> Context (DarkGray)
        assert_eq!(lines[6].spans[0].content, " context_line();");
        assert_eq!(lines[6].spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn test_clarification_prompt_state_navigation() {
        let decision = JevRoutingDecision {
            tool: SidekickTaskKind::AstSearch,
            confidence: 0.65,
            confidence_level: ConfidenceLevel::Medium,
            probabilities: std::collections::HashMap::new(),
            model: "mock-jev".into(),
            reasoning: "Ambiguous symbol query".into(),
            is_fallback: false,
        };

        let mut prompt =
            ClarificationPromptState::from_decision("find where Sidekick is defined", &decision);
        assert_eq!(prompt.suggested_tool, SidekickTaskKind::AstSearch);
        assert_eq!(prompt.confidence_level, ConfidenceLevel::Medium);
        assert_eq!(prompt.selected_tool(), SidekickTaskKind::AstSearch);

        // Test next navigation wraps through CLARIFICATION_CANDIDATE_TOOLS
        let total = CLARIFICATION_CANDIDATE_TOOLS.len();
        let initial_idx = prompt.selected_idx;

        prompt.next();
        assert_eq!(prompt.selected_idx, (initial_idx + 1) % total);

        // Navigate backwards
        prompt.prev();
        assert_eq!(prompt.selected_idx, initial_idx);

        // Wrap around backwards from 0
        prompt.selected_idx = 0;
        prompt.prev();
        assert_eq!(prompt.selected_idx, total - 1);

        // Wrap around forwards from last
        prompt.next();
        assert_eq!(prompt.selected_idx, 0);
    }

    #[test]
    fn test_task_for_tool_mapping() {
        for &tool in CLARIFICATION_CANDIDATE_TOOLS {
            let task = task_for_tool(tool, "sample prompt", 42);
            assert_eq!(task.kind, tool);
            assert_eq!(task.orchestrator, "Jev System One");
            assert!(task.id.contains("42"));
        }
    }

    #[test]
    fn test_render_clarification_dialog_standard_and_wide() {
        let decision = JevRoutingDecision {
            tool: SidekickTaskKind::SemanticSearch,
            confidence: 0.45,
            confidence_level: ConfidenceLevel::Low,
            probabilities: std::collections::HashMap::new(),
            model: "mock-jev".into(),
            reasoning: "Search keyword matched".into(),
            is_fallback: false,
        };
        let prompt = ClarificationPromptState::from_decision("where is my invoice", &decision);

        // Test on standard 80x24 terminal
        let backend_80 = TestBackend::new(80, 24);
        let mut terminal_80 = Terminal::new(backend_80).unwrap();
        let engine = SidekickEngine::new();

        terminal_80
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 0, None, Some(&prompt));
            })
            .unwrap();

        let buffer_80 = terminal_80.backend().buffer();
        let content_80: String = buffer_80.content().iter().map(|c| c.symbol()).collect();
        assert!(content_80.contains("CLARIFICATION PAUSED"));
        assert!(content_80.contains("where is my invoice"));
        assert!(content_80.contains("Low"));
        assert!(content_80.contains("Candidate Tools"));

        // Test on wide 160x40 terminal
        let backend_160 = TestBackend::new(160, 40);
        let mut terminal_160 = Terminal::new(backend_160).unwrap();

        terminal_160
            .draw(|f| {
                render_sidekick_dashboard(f, &engine, 0, None, Some(&prompt));
            })
            .unwrap();

        let buffer_160 = terminal_160.backend().buffer();
        let content_160: String = buffer_160.content().iter().map(|c| c.symbol()).collect();
        assert!(content_160.contains("CLARIFICATION PAUSED"));
        assert!(content_160.contains("where is my invoice"));
    }
}
