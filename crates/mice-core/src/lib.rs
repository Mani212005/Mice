use std::{
    fs,
    path::{Path, PathBuf},
};

use mice_providers::{Action, CostPolicy, DEFAULT_CLOUD_MODEL, DEFAULT_LOCAL_MODEL, PrivacyMode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub privacy_mode: PrivacyMode,
    #[serde(default)]
    pub cost_policy: CostPolicy,
    #[serde(default = "default_cloud_model")]
    pub cloud_model: String,
    #[serde(default = "default_local_model")]
    pub local_model: String,
    #[serde(default)]
    pub gesture: GestureConfig,
    #[serde(default)]
    pub autopilot: AutopilotConfig,
}

fn default_cloud_model() -> String {
    DEFAULT_CLOUD_MODEL.into()
}
fn default_local_model() -> String {
    DEFAULT_LOCAL_MODEL.into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            privacy_mode: PrivacyMode::CloudAllowed,
            cost_policy: CostPolicy::Cheapest,
            cloud_model: default_cloud_model(),
            local_model: default_local_model(),
            gesture: GestureConfig::default(),
            autopilot: AutopilotConfig::default(),
        }
    }
}

/// Autopilot stays deliberately conservative by default. These preferences
/// are configuration only; no browser content or credentials are stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutopilotConfig {
    #[serde(default = "default_autopilot_persona")]
    pub persona: String,
    #[serde(default = "default_autopilot_careful_mode")]
    pub careful_mode: bool,
    #[serde(default = "default_autopilot_first_run")]
    pub first_run: bool,
}

fn default_autopilot_persona() -> String {
    "patient".into()
}
fn default_autopilot_careful_mode() -> bool {
    false
}
fn default_autopilot_first_run() -> bool {
    true
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            persona: default_autopilot_persona(),
            careful_mode: default_autopilot_careful_mode(),
            first_run: default_autopilot_first_run(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GestureConfig {
    #[serde(default = "default_trigger")]
    pub trigger: String,
    #[serde(default = "default_chord_window")]
    pub chord_window_ms: u64,
    #[serde(default = "default_hold_threshold")]
    pub hold_threshold_ms: u64,
    #[serde(default = "default_summarize_selection_trigger")]
    pub summarize_selection_trigger: String,
    #[serde(default = "default_infographic_selection_trigger")]
    pub infographic_selection_trigger: String,
    #[serde(default = "default_goal_trigger")]
    pub goal_trigger: String,
}
fn default_trigger() -> String {
    "ctrl+shift+space".into()
}
fn default_chord_window() -> u64 {
    120
}
fn default_hold_threshold() -> u64 {
    350
}
fn default_summarize_selection_trigger() -> String {
    "ctrl-double-tap".into()
}
fn default_infographic_selection_trigger() -> String {
    "ctrl+alt+i".into()
}
fn default_goal_trigger() -> String {
    "ctrl+alt+space".into()
}
impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            trigger: default_trigger(),
            chord_window_ms: default_chord_window(),
            hold_threshold_ms: default_hold_threshold(),
            summarize_selection_trigger: default_summarize_selection_trigger(),
            infographic_selection_trigger: default_infographic_selection_trigger(),
            goal_trigger: default_goal_trigger(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Could not read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("Could not serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/MICE/config.toml"))
}

pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Ok(Config::default());
    }
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

pub fn save_config(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    fs::write(path, toml::to_string_pretty(config)?)?;
    Ok(())
}

pub fn default_config_toml() -> &'static str {
    "privacy_mode = \"cloud_allowed\"\ncost_policy = \"cheapest\"\ncloud_model = \"gpt-5.6-luna\"\n# Safe default: gemma3:4b. Alternatives: phi4-mini, gpt-oss:20b (heavy opt-in only).\nlocal_model = \"gemma3:4b\"\n\n[autopilot]\npersona = \"patient\"\n# The first completed goal confirms each safe action, then this turns off automatically.\nfirst_run = true\n# Set true to keep per-action confirmation for every future goal.\ncareful_mode = false\n\n[gesture]\ntrigger = \"ctrl+shift+space\"\nchord_window_ms = 120\nhold_threshold_ms = 350\nsummarize_selection_trigger = \"ctrl-double-tap\"\ninfographic_selection_trigger = \"ctrl+alt+i\"\ngoal_trigger = \"ctrl+alt+space\"\n"
}

/// Portable, side-effect-free state for the first Goal Guide stage. Platform
/// prompts and model calls stay outside this type; it only guards the allowed
/// review flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalState {
    AwaitingGoal,
    Planning { goal: String },
    Reviewing { goal: String, plan: String },
    Accepted { goal: String, plan: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalSession {
    state: GoalState,
}

impl GoalSession {
    pub fn new() -> Self {
        Self {
            state: GoalState::AwaitingGoal,
        }
    }

    pub fn submit_goal(&mut self, goal: String) -> Result<(), &'static str> {
        if goal.trim().is_empty() {
            return Err("Enter a goal before asking MICE to make a plan.");
        }
        if !matches!(self.state, GoalState::AwaitingGoal) {
            return Err("This goal session is not waiting for a goal.");
        }
        self.state = GoalState::Planning { goal };
        Ok(())
    }

    pub fn begin_revision(&mut self, revision: String) -> Result<String, &'static str> {
        let GoalState::Reviewing { goal, plan } = &self.state else {
            return Err("This goal session has no plan to revise.");
        };
        let goal = goal.clone();
        let plan = plan.clone();
        self.state = GoalState::Planning { goal: goal.clone() };
        Ok(format!(
            "Original goal: {goal}\n\nCurrent plan:\n{plan}\n\nRequested revision: {revision}"
        ))
    }

    pub fn review(&mut self, plan: String) -> Result<(), &'static str> {
        let GoalState::Planning { goal } = &self.state else {
            return Err("This goal session is not planning.");
        };
        self.state = GoalState::Reviewing {
            goal: goal.clone(),
            plan,
        };
        Ok(())
    }

    pub fn accept(&mut self) -> Result<(), &'static str> {
        let GoalState::Reviewing { goal, plan } = &self.state else {
            return Err("This goal session has no plan to accept.");
        };
        self.state = GoalState::Accepted {
            goal: goal.clone(),
            plan: plan.clone(),
        };
        Ok(())
    }

    pub fn state(&self) -> &GoalState {
        &self.state
    }
}

impl Default for GoalSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Portable state for the cloud-driven M12 browser loop. It deliberately owns
/// no I/O: the CLI supplies observations and executes validated decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Autopilot,
    Guide,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAction {
    Click,
    Fill,
    OpenUrl,
    Scroll,
    Done,
    Handoff,
    AskUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentDecision {
    pub say_to_user: String,
    pub action: AgentAction,
    pub candidate_id: Option<String>,
    pub url: Option<String>,
    pub value: Option<String>,
    pub done_summary: Option<String>,
    pub question: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactTurn {
    pub action: String,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLoopState {
    Running,
    Paused(String),
    HandedOff(String),
    Done(String),
    Stopped,
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLoop {
    pub goal: String,
    pub mode: AgentMode,
    pub history: Vec<CompactTurn>,
    pub max_actions: usize,
    pub actions_taken: usize,
    pub state: AgentLoopState,
    pub last_action_target: Option<String>,
    pub last_failed_target: Option<String>,
    pub consecutive_failures: usize,
}

impl AgentLoop {
    pub fn new(goal: String, mode: AgentMode, max_actions: usize) -> Self {
        Self {
            goal,
            mode,
            history: Vec::new(),
            max_actions,
            actions_taken: 0,
            state: AgentLoopState::Running,
            last_action_target: None,
            last_failed_target: None,
            consecutive_failures: 0,
        }
    }
    pub fn apply_decision(&mut self, decision: &AgentDecision) -> Result<(), &'static str> {
        if !matches!(self.state, AgentLoopState::Running) {
            return Err("Agent loop is not running.");
        }
        match decision.action {
            AgentAction::Done => {
                self.state = AgentLoopState::Done(
                    decision
                        .done_summary
                        .clone()
                        .unwrap_or_else(|| decision.say_to_user.clone()),
                )
            }
            AgentAction::AskUser => {
                self.state = AgentLoopState::Paused(
                    decision
                        .question
                        .clone()
                        .unwrap_or_else(|| decision.say_to_user.clone()),
                )
            }
            AgentAction::Handoff => {
                self.state = AgentLoopState::HandedOff(decision.say_to_user.clone())
            }
            _ => {
                self.actions_taken += 1;
                if self.actions_taken > self.max_actions {
                    self.state = AgentLoopState::BudgetExhausted;
                }
            }
        }
        Ok(())
    }
    pub fn record(&mut self, action: impl Into<String>, result: impl Into<String>) {
        self.history.push(CompactTurn {
            action: action.into(),
            result: result.into(),
        });
        if self.history.len() > 15 {
            self.history.remove(0);
        }
    }
    /// A browser action can transiently fail as a page redraws. Two failures
    /// against the same verified target end automation and hand control back
    /// to the person rather than retrying blindly.
    pub fn record_action_result(&mut self, target: Option<String>, success: bool, result: &str) {
        if success {
            self.last_failed_target = None;
            self.consecutive_failures = 0;
            self.merge_latest_action_result(result);
            return;
        }
        if target.is_some() && target == self.last_failed_target {
            self.consecutive_failures += 1;
        } else {
            self.last_failed_target = target;
            self.consecutive_failures = 1;
        }
        self.merge_latest_action_result(result);
        if self.consecutive_failures >= 2 {
            self.state = AgentLoopState::HandedOff(
                "That control failed twice. Please do this one yourself, then I can continue."
                    .into(),
            );
        }
    }
    fn merge_latest_action_result(&mut self, result: &str) {
        if let Some(turn) = self.history.last_mut()
            && matches!(
                turn.action.as_str(),
                "click" | "fill" | "open_url" | "scroll"
            )
        {
            turn.result = format!("{}; {result}", turn.result);
        } else {
            self.record("action result", result);
        }
    }
    pub fn stop(&mut self) {
        self.state = AgentLoopState::Stopped;
    }
}

/// Every clipboard representation is derived from the same model response.
/// The portable core decides the representations; the platform agent writes
/// them to its native clipboard implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardContents {
    pub text: String,
    pub html: String,
    pub rtf: String,
}

pub fn clipboard_contents(text: &str) -> ClipboardContents {
    ClipboardContents {
        text: text.into(),
        html: markdown_table_html(text).unwrap_or_else(|| {
            format!(
                "<html><body>{}</body></html>",
                html_escape(text).replace('\n', "<br>\n")
            )
        }),
        rtf: format!("{{\\rtf1\\ansi\\deff0 {} }}", rtf_escape(text)),
    }
}

/// Turns a selected preset into a concise, deterministic instruction. The
/// caller's words are retained as additional context instead of being silently
/// discarded by the preset.
pub fn action_instruction(action: Action, instruction: &str) -> String {
    let directive = match action {
        Action::Explain => "Explain the selected content clearly and concisely.",
        Action::Summarize => "Summarize the selected content with its key points.",
        Action::Rewrite => "Rewrite the selected content, preserving its meaning.",
        Action::Translate => {
            "Translate the selected content. State the target language if one is provided."
        }
        Action::ExtractJson => "Extract the selected content into valid JSON only.",
        Action::Code => "Produce a correct code-focused answer for the selected content.",
        Action::Image => "Create an infographic from the selected content.",
        Action::Guide => "Guide the user to the requested UI element.",
        Action::GoalPlan => "Create a safe, advisory plan for the user's goal.",
        Action::Qa => "Answer the question using the selected content as context.",
        Action::Define => {
            "Define the selected word or phrase concisely: give its meaning, part of speech, and a short example sentence. If it has two or three common senses, list them briefly."
        }
    };
    if instruction.trim().is_empty() {
        directive.into()
    } else {
        format!("{directive}\n\nAdditional request: {instruction}")
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Convert a simple Markdown table into semantic HTML so spreadsheet and rich
/// text destinations can select the table representation from the clipboard.
fn markdown_table_html(value: &str) -> Option<String> {
    let lines = value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let separator_index = lines
        .iter()
        .position(|line| markdown_table_separator(line))?;
    if separator_index == 0 || separator_index + 1 >= lines.len() {
        return None;
    }
    let headers = markdown_table_cells(lines[separator_index - 1])?;
    let rows = lines[separator_index + 1..]
        .iter()
        .map(|line| markdown_table_cells(line))
        .collect::<Option<Vec<_>>>()?;
    if rows.iter().any(|row| row.len() != headers.len()) {
        return None;
    }
    let header_html = headers
        .iter()
        .map(|cell| format!("<th>{}</th>", html_escape(cell)))
        .collect::<String>();
    let rows_html = rows
        .iter()
        .map(|row| {
            let cells = row
                .iter()
                .map(|cell| format!("<td>{}</td>", html_escape(cell)))
                .collect::<String>();
            format!("<tr>{cells}</tr>")
        })
        .collect::<String>();
    Some(format!(
        "<html><body><table><thead><tr>{header_html}</tr></thead><tbody>{rows_html}</tbody></table></body></html>"
    ))
}

fn markdown_table_separator(line: &str) -> bool {
    markdown_table_cells(line).is_some_and(|cells| {
        !cells.is_empty()
            && cells.iter().all(|cell| {
                let trimmed = cell.trim().trim_matches(':');
                trimmed.len() >= 3 && trimmed.chars().all(|character| character == '-')
            })
    })
}

fn markdown_table_cells(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let trimmed = trimmed.trim_matches('|');
    let cells = trimmed.split('|').map(str::trim).collect::<Vec<_>>();
    (!cells.is_empty()).then_some(cells)
}

fn rtf_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('\n', "\\line\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_deserialize() {
        let config = toml::from_str::<Config>("").unwrap();
        assert_eq!(config.cloud_model, DEFAULT_CLOUD_MODEL);
        assert_eq!(config.local_model, DEFAULT_LOCAL_MODEL);
        assert_eq!(config.gesture.trigger, "ctrl+shift+space");
        assert_eq!(
            config.gesture.summarize_selection_trigger,
            "ctrl-double-tap"
        );
        assert_eq!(config.gesture.infographic_selection_trigger, "ctrl+alt+i");
        assert_eq!(config.gesture.goal_trigger, "ctrl+alt+space");
    }

    #[test]
    fn goal_session_requires_review_before_accepting() {
        let mut session = GoalSession::new();
        assert!(session.accept().is_err());
        session.submit_goal("Organize my files".into()).unwrap();
        session.review("1. Open Finder".into()).unwrap();
        assert!(session.begin_revision("Make it shorter".into()).is_ok());
        session.review("1. Open Finder".into()).unwrap();
        session.accept().unwrap();
        assert!(matches!(session.state(), GoalState::Accepted { .. }));
    }

    #[test]
    fn agent_loop_pauses_handoffs_and_enforces_its_action_budget() {
        let mut loop_state = AgentLoop::new("Open Canva".into(), AgentMode::Autopilot, 1);
        let click = AgentDecision {
            say_to_user: "Opening search".into(),
            action: AgentAction::Click,
            candidate_id: Some("candidate-1".into()),
            url: None,
            value: None,
            done_summary: None,
            question: None,
        };
        loop_state.apply_decision(&click).unwrap();
        loop_state.record("click", "opened search");
        assert!(matches!(loop_state.state, AgentLoopState::Running));
        loop_state.apply_decision(&click).unwrap();
        assert!(matches!(loop_state.state, AgentLoopState::BudgetExhausted));
        let mut handoff = AgentLoop::new("Log in".into(), AgentMode::Autopilot, 3);
        let decision = AgentDecision {
            say_to_user: "Please type your password.".into(),
            action: AgentAction::Handoff,
            candidate_id: None,
            url: None,
            value: None,
            done_summary: None,
            question: None,
        };
        handoff.apply_decision(&decision).unwrap();
        assert!(matches!(handoff.state, AgentLoopState::HandedOff(_)));
    }

    #[test]
    fn agent_loop_handoffs_after_two_failures_on_the_same_target() {
        let mut loop_state = AgentLoop::new("Open Canva".into(), AgentMode::Autopilot, 15);
        loop_state.record("click", "sent to Canva");
        loop_state.record_action_result(Some("#canva-result".into()), false, "not visible");
        assert!(matches!(loop_state.state, AgentLoopState::Running));
        assert_eq!(loop_state.history.len(), 1);
        loop_state.record_action_result(Some("#canva-result".into()), false, "not visible");
        assert!(matches!(loop_state.state, AgentLoopState::HandedOff(_)));
    }

    #[test]
    fn agent_decision_deserializes_provider_snake_case_json() {
        let decision: AgentDecision = serde_json::from_str(
            r#"{"say_to_user":"Opening Canva.","action":"click","candidate_id":"candidate-3","url":null,"value":null,"done_summary":null,"question":null}"#,
        )
        .unwrap();
        assert_eq!(decision.say_to_user, "Opening Canva.");
        assert_eq!(decision.candidate_id.as_deref(), Some("candidate-3"));
        assert_eq!(decision.action, AgentAction::Click);
    }

    #[test]
    fn clipboard_contents_preserve_plain_text_and_escape_rich_forms() {
        let contents = clipboard_contents("One <two> & {three}\nFour");
        assert_eq!(contents.text, "One <two> & {three}\nFour");
        assert!(contents.html.contains("&lt;two&gt; &amp; {three}<br>"));
        assert!(contents.rtf.contains("\\{three\\}\\line"));
    }

    #[test]
    fn action_instruction_keeps_user_context() {
        let instruction = action_instruction(Action::Translate, "into French");
        assert!(instruction.contains("Translate"));
        assert!(instruction.contains("into French"));
    }

    #[test]
    fn clipboard_html_preserves_markdown_tables_for_spreadsheets() {
        let contents = clipboard_contents("| Name | Score |\n| --- | ---: |\n| Ada | 10 |");
        assert_eq!(
            contents.html,
            "<html><body><table><thead><tr><th>Name</th><th>Score</th></tr></thead><tbody><tr><td>Ada</td><td>10</td></tr></tbody></table></body></html>"
        );
    }
}
