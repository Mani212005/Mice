use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use mice_providers::{Action, CostPolicy, DEFAULT_CLOUD_MODEL, DEFAULT_LOCAL_MODEL, PrivacyMode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Model-backed planners may produce a task graph, but the portable core
/// keeps that proposal bounded before it can become a launchable mission.
pub const MAX_MISSION_TASKS: usize = 24;

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
    /// Model reserved for short local tool loops. It is selected by the
    /// machine profile and never replaces the privacy-first summary model.
    #[serde(default = "default_tool_model")]
    pub tool_model: String,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub machine_profile: MachineProfile,
    #[serde(default)]
    pub gesture: GestureConfig,
    #[serde(default)]
    pub autopilot: AutopilotConfig,
    #[serde(default)]
    pub mcp: McpConfig,
}

/// External MCP servers the user has explicitly granted. MICE only ever
/// spawns a server that appears here with `enabled = true`; nothing is
/// discovered or connected automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The explicit grant: a configured but disabled server is never spawned.
    #[serde(default)]
    pub enabled: bool,
}

fn default_cloud_model() -> String {
    DEFAULT_CLOUD_MODEL.into()
}
fn default_local_model() -> String {
    DEFAULT_LOCAL_MODEL.into()
}
fn default_tool_model() -> String {
    "phi4-mini".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            privacy_mode: PrivacyMode::CloudAllowed,
            cost_policy: CostPolicy::Cheapest,
            cloud_model: default_cloud_model(),
            local_model: default_local_model(),
            tool_model: default_tool_model(),
            routing: RoutingConfig::default(),
            machine_profile: MachineProfile::default(),
            gesture: GestureConfig::default(),
            autopilot: AutopilotConfig::default(),
            mcp: McpConfig::default(),
        }
    }
}

/// Harnesses Mission Control can reason about. An adapter is still required
/// before any harness can be launched; this enum deliberately does not imply
/// a shell command or permission to start a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionAgentKind {
    Codex,
    Claude,
    Antigravity,
}

impl MissionAgentKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude" | "claude-code" | "claude_code" => Some(Self::Claude),
            "antigravity" => Some(Self::Antigravity),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Antigravity => "Antigravity",
        }
    }
}

/// A probe result, not an execution grant. The controller may show these
/// fields in a dry run but must require an explicit launch adapter later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionAgentCapability {
    pub agent: MissionAgentKind,
    pub installed: bool,
    pub mcp_available: bool,
    pub launch_ready: bool,
    pub detail: String,
}

/// Metadata-only identity for a mission. `repo_id` and `plan_fingerprint` are
/// hashes; the plan body and user configuration are not copied into MICE's
/// coordination state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionIdentity {
    pub repo_id: String,
    pub mission_id: String,
    pub plan_fingerprint: String,
}

/// A validated work unit proposed from a repository plan. The M0 parser may
/// populate conservative candidates from headings; later model planners must
/// still pass this deterministic validation before a task can be assigned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub predicted_paths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTaskGraph {
    #[serde(default)]
    pub tasks: Vec<MissionTask>,
}

/// Lifecycle facts MICE can safely retain for a launched task. A process
/// exiting is deliberately not a success state: completion must later be
/// backed by an explicit report and verification evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTaskState {
    Proposed,
    Running,
    ExitedUnreported,
    ReportedReady,
    VerifiedReady,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTaskRuntime {
    pub task_id: String,
    pub agent: MissionAgentKind,
    pub state: MissionTaskState,
    pub branch: String,
    /// A worktree checkout path is operational metadata, never repository
    /// source or an agent transcript. Mission storage itself stays outside the
    /// Git working tree with owner-only permissions.
    pub worktree_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    /// The adapter runner's bounded process exit code, when it could be
    /// recovered. This is lifecycle evidence only; MICE never stores worker
    /// output or transcripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Git-derived changed paths from the isolated worktree. This is bounded
    /// operational context, not a diff or an agent transcript.
    #[serde(default)]
    pub observed_paths: Vec<String>,
    /// Short, explicit coordination notes. MICE bounds and redacts these
    /// before persistence; they are never a channel for full agent output.
    #[serde(default)]
    pub coordination_notes: Vec<String>,
    /// A timestamp means MICE has verified basic Git evidence for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<u64>,
    pub started_at: u64,
}

/// An approved task-to-harness mapping. This is persisted with the validated
/// graph so later lifecycle commands cannot silently re-plan a live mission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTaskAssignment {
    pub task_id: String,
    pub agent: MissionAgentKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionRecord {
    pub identity: MissionIdentity,
    pub updated_at: u64,
    /// The graph that passed validation at the first launch boundary. Older
    /// records deserialize to an empty graph and retain their legacy parser
    /// fallback instead of becoming unreadable.
    #[serde(default)]
    pub graph: MissionTaskGraph,
    #[serde(default)]
    pub assignments: Vec<MissionTaskAssignment>,
    #[serde(default)]
    pub tasks: Vec<MissionTaskRuntime>,
}

impl MissionTaskGraph {
    /// Ensure task proposals are complete and safe to schedule. This is
    /// intentionally model-independent: invalid JSON from a planner must not
    /// become a launchable mission merely because it looks plausible.
    pub fn validate(&self) -> Result<(), String> {
        if self.tasks.is_empty() {
            return Err("A mission needs at least one task.".into());
        }
        if self.tasks.len() > MAX_MISSION_TASKS {
            return Err(format!(
                "A mission supports at most {MAX_MISSION_TASKS} tasks in one review."
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            if !is_valid_mission_id(&task.id) {
                return Err(format!("Task ID `{}` is invalid.", task.id));
            }
            if !task_ids.insert(task.id.as_str()) {
                return Err(format!("Task ID `{}` appears more than once.", task.id));
            }
            if task.title.trim().is_empty() || task.title.chars().count() > 160 {
                return Err(format!(
                    "Task `{}` needs a title of at most 160 characters.",
                    task.id
                ));
            }
            if task.acceptance.iter().all(|item| item.trim().is_empty()) {
                return Err(format!(
                    "Task `{}` needs at least one acceptance check.",
                    task.id
                ));
            }
            let mut dependencies = BTreeSet::new();
            for dependency in &task.dependencies {
                if dependency == &task.id {
                    return Err(format!("Task `{}` cannot depend on itself.", task.id));
                }
                if !dependencies.insert(dependency.as_str()) {
                    return Err(format!(
                        "Task `{}` lists dependency `{dependency}` more than once.",
                        task.id
                    ));
                }
            }
            for path in &task.predicted_paths {
                if !is_safe_predicted_path(path) {
                    return Err(format!(
                        "Task `{}` has an unsafe predicted path `{path}`.",
                        task.id
                    ));
                }
            }
        }
        let known = task_ids;
        for task in &self.tasks {
            for dependency in &task.dependencies {
                if !known.contains(dependency.as_str()) {
                    return Err(format!(
                        "Task `{}` depends on unknown task `{dependency}`.",
                        task.id
                    ));
                }
            }
        }
        let mut remaining = self
            .tasks
            .iter()
            .map(|task| {
                (
                    task.id.as_str(),
                    task.dependencies.iter().collect::<BTreeSet<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut scheduled = BTreeSet::new();
        while let Some(next) = remaining
            .iter()
            .find_map(|(id, dependencies)| dependencies.is_empty().then_some(*id))
        {
            remaining.remove(next);
            scheduled.insert(next);
            for dependencies in remaining.values_mut() {
                dependencies.retain(|dependency| *dependency != next);
            }
        }
        if scheduled.len() != self.tasks.len() {
            return Err("Mission task dependencies contain a cycle.".into());
        }
        for (left_index, left) in self.tasks.iter().enumerate() {
            for right in self.tasks.iter().skip(left_index + 1) {
                // A dependency chain makes the tasks serial, so the shared
                // scope is safe. Otherwise they may be given to separate
                // agents concurrently and must not claim the same file (or
                // a file beneath a claimed directory).
                if task_depends_on(&self.tasks, &left.id, &right.id)
                    || task_depends_on(&self.tasks, &right.id, &left.id)
                {
                    continue;
                }
                if let Some((left_path, right_path)) = left
                    .predicted_paths
                    .iter()
                    .flat_map(|left_path| {
                        right
                            .predicted_paths
                            .iter()
                            .map(move |right_path| (left_path, right_path))
                    })
                    .find(|(left_path, right_path)| predicted_paths_overlap(left_path, right_path))
                {
                    return Err(format!(
                        "Tasks `{}` and `{}` have overlapping predicted paths `{left_path}` and `{right_path}` without a dependency.",
                        left.id, right.id
                    ));
                }
            }
        }
        Ok(())
    }
}

fn task_depends_on(tasks: &[MissionTask], task_id: &str, predecessor: &str) -> bool {
    let mut pending = tasks
        .iter()
        .find(|task| task.id == task_id)
        .into_iter()
        .flat_map(|task| task.dependencies.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some(next) = pending.pop() {
        if next == predecessor {
            return true;
        }
        if !visited.insert(next) {
            continue;
        }
        if let Some(task) = tasks.iter().find(|task| task.id == next) {
            pending.extend(task.dependencies.iter().map(String::as_str));
        }
    }
    false
}

fn is_valid_mission_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_safe_predicted_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\0')
        && !value.starts_with('/')
        && !value.starts_with('~')
        && !value.contains('\\')
        && !starts_with_windows_drive(value)
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn starts_with_windows_drive(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn predicted_paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|remainder| remainder.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

/// Machine capability controls which execution lanes are trustworthy. A light
/// machine distils locally but does not attempt an unreliable local tool loop.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineProfile {
    #[default]
    Light,
    Standard,
    Heavy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "enabled")]
    pub deterministic: bool,
    #[serde(default = "enabled")]
    pub local: bool,
    #[serde(default = "enabled")]
    pub cheap_cloud: bool,
    #[serde(default = "enabled")]
    pub frontier: bool,
    #[serde(default = "default_quota_bias")]
    pub quota_bias_percent: u8,
}

fn enabled() -> bool {
    true
}
fn default_quota_bias() -> u8 {
    80
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            deterministic: true,
            local: true,
            cheap_cloud: true,
            frontier: true,
            quota_bias_percent: default_quota_bias(),
        }
    }
}

/// The execution-cost ladder, ordered from work that needs no model to the
/// most expensive fresh frontier call. The registry uses this before asking an
/// SLM to reason about a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLane {
    Deterministic,
    Local,
    CheapCloud,
    Frontier,
    Unavailable,
}

pub fn route_execution_lane(
    profile: MachineProfile,
    routing: &RoutingConfig,
    deterministic_available: bool,
    requires_loop: bool,
    quota_percent: Option<u8>,
) -> ExecutionLane {
    if deterministic_available && routing.deterministic {
        return ExecutionLane::Deterministic;
    }
    let quota_biased = quota_percent.is_some_and(|value| value >= routing.quota_bias_percent);
    if routing.local && (!requires_loop || profile != MachineProfile::Light) {
        return ExecutionLane::Local;
    }
    if !quota_biased && routing.cheap_cloud {
        return ExecutionLane::CheapCloud;
    }
    if !quota_biased && routing.frontier {
        return ExecutionLane::Frontier;
    }
    ExecutionLane::Unavailable
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
    #[serde(default = "default_smart_copy_trigger")]
    pub smart_copy_trigger: String,
    #[serde(default = "default_palette_trigger")]
    pub palette_trigger: String,
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
fn default_smart_copy_trigger() -> String {
    "ctrl+alt+c".into()
}
fn default_palette_trigger() -> String {
    "ctrl+shift+space".into()
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
            smart_copy_trigger: default_smart_copy_trigger(),
            palette_trigger: default_palette_trigger(),
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
    #[error("Unsupported smart-copy trigger {0:?}; use ctrl+alt+c or ctrl+alt+x")]
    InvalidSmartCopyTrigger(String),
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/MICE/config.toml"))
}

pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let config = toml::from_str(&fs::read_to_string(path)?)?;
    validate_config(&config)?;
    Ok(config)
}

pub fn save_config(path: &Path, config: &Config) -> Result<(), ConfigError> {
    validate_config(config)?;
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

fn validate_config(config: &Config) -> Result<(), ConfigError> {
    match config.gesture.smart_copy_trigger.as_str() {
        "ctrl+alt+c" | "ctrl+alt+x" => Ok(()),
        value => Err(ConfigError::InvalidSmartCopyTrigger(value.to_owned())),
    }
}

/// Non-fatal configuration problems, surfaced at `mice start` and
/// `mice doctor`. Unlike `validate_config`, none of these block loading: a
/// typo'd model or trigger degrades one feature, and refusing to start would
/// hide the message behind the failure it caused.
pub fn config_warnings(config: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for (field, model) in [
        ("local_model", &config.local_model),
        ("cloud_model", &config.cloud_model),
        ("tool_model", &config.tool_model),
    ] {
        if mice_providers::model_descriptor(model).is_none() {
            warnings.push(format!(
                "{field} `{model}` is not a model MICE knows; that lane will fail until it is corrected in `mice settings`"
            ));
        }
    }
    let supported: [(&str, &String, &[&str]); 5] = [
        (
            "gesture.trigger",
            &config.gesture.trigger,
            &["ctrl+shift+space", "ctrl+alt+space", "cmd+shift+space"],
        ),
        (
            "gesture.summarize_selection_trigger",
            &config.gesture.summarize_selection_trigger,
            &["ctrl-double-tap", "ctrl+alt+s"],
        ),
        (
            "gesture.infographic_selection_trigger",
            &config.gesture.infographic_selection_trigger,
            &["ctrl+alt+i", "ctrl+alt+m"],
        ),
        (
            "gesture.goal_trigger",
            &config.gesture.goal_trigger,
            &["ctrl+alt+space"],
        ),
        (
            "gesture.palette_trigger",
            &config.gesture.palette_trigger,
            &["ctrl+shift+space", "ctrl+alt+space", "cmd+shift+space"],
        ),
    ];
    for (field, value, options) in supported {
        if !options.contains(&value.as_str()) {
            warnings.push(format!(
                "{field} `{value}` is not supported (expected one of {}); that gesture will not fire",
                options.join(", ")
            ));
        }
    }
    if !(30..=1_000).contains(&config.gesture.chord_window_ms) {
        warnings.push(format!(
            "gesture.chord_window_ms {} is outside the usable 30–1000 ms range",
            config.gesture.chord_window_ms
        ));
    }
    if !(100..=5_000).contains(&config.gesture.hold_threshold_ms) {
        warnings.push(format!(
            "gesture.hold_threshold_ms {} is outside the usable 100–5000 ms range",
            config.gesture.hold_threshold_ms
        ));
    }
    if config.routing.quota_bias_percent > 100 {
        warnings.push(format!(
            "routing.quota_bias_percent {} exceeds 100 and disables quota biasing",
            config.routing.quota_bias_percent
        ));
    }
    if config.gesture.palette_trigger == config.gesture.goal_trigger {
        warnings.push(
            "gesture.palette_trigger conflicts with goal_trigger; the Goal shortcut takes precedence and opens the palette prefilled with `plan `"
                .into(),
        );
    } else if config.gesture.palette_trigger == config.gesture.trigger
        && config.gesture.palette_trigger != "ctrl+shift+space"
    {
        warnings.push(
            "gesture.palette_trigger conflicts with the legacy capture trigger; the palette takes precedence"
                .into(),
        );
    }
    let mut seen_servers = std::collections::BTreeSet::new();
    for server in &config.mcp.servers {
        if server.name.trim().is_empty() || server.command.trim().is_empty() {
            warnings.push(
                "an [[mcp.servers]] entry is missing a name or command and will be ignored".into(),
            );
        } else if !seen_servers.insert(server.name.as_str()) {
            warnings.push(format!(
                "mcp server name `{}` appears more than once; only the first is used",
                server.name
            ));
        }
    }
    warnings
}

pub fn default_config_toml() -> String {
    r#"privacy_mode = "cloud_allowed"
cost_policy = "cheapest"
cloud_model = "gpt-5.6-luna"
# Safe default: gemma3:4b. Alternatives: phi4-mini, gpt-oss:20b (heavy opt-in only).
local_model = "gemma3:4b"
# Tool-loop model; light machines automatically keep this lane disabled.
tool_model = "phi4-mini"
machine_profile = "light"

[routing]
deterministic = true
local = true
cheap_cloud = true
frontier = true
# Bias away from paid lanes once quota-axi reports this percent of a window used.
quota_bias_percent = 80

[autopilot]
persona = "patient"
# The first completed goal confirms each safe action, then this turns off automatically.
first_run = true
# Set true to keep per-action confirmation for every future goal.
careful_mode = false

[gesture]
trigger = "ctrl+shift+space"
chord_window_ms = 120
hold_threshold_ms = 350
summarize_selection_trigger = "ctrl-double-tap"
infographic_selection_trigger = "ctrl+alt+i"
goal_trigger = "ctrl+alt+space"
# After a normal Cmd-C, this gesture asks MICE to enrich the copied content.
smart_copy_trigger = "ctrl+alt+c"
# Unified command palette (daemon mode).
palette_trigger = "ctrl+shift+space"

# External MCP servers require an explicit grant: add an entry AND set
# enabled = true. MICE spawns them with a scrubbed environment (no provider
# keys) and treats their output as untrusted text.
# [[mcp.servers]]
# name = "web-search"
# command = "/usr/local/bin/my-search-mcp"
# args = []
# enabled = false
"#
    .into()
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

    /// End a goal before it starts. Cancellation is deliberately available
    /// only before acceptance so an active guide always has an explicit Quit
    /// control instead of silently changing its planning state.
    pub fn cancel(&mut self) -> Result<(), &'static str> {
        if !matches!(
            self.state,
            GoalState::AwaitingGoal | GoalState::Reviewing { .. }
        ) {
            return Err("This goal session cannot be cancelled at this stage.");
        }
        self.state = GoalState::Cancelled;
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

/// Action to perform when a scheduled task or reminder triggers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    Reminder { message: String },
    ExecuteGoal { goal: String, plan: Option<String> },
}

/// A background task or reminder scheduled for execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: String,
    pub created_at: u64,
    pub trigger_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron_expression: Option<String>,
    pub action: ScheduleAction,
    pub triggered: bool,
}

pub fn parse_schedule_time(input: &str, relative_from: u64) -> Result<u64, String> {
    let mut input = input.trim().to_lowercase();
    if input.is_empty() {
        return Err("Schedule time specification cannot be empty.".into());
    }
    if let Some(rest) = input.strip_prefix("in ") {
        input = rest.trim().to_owned();
    }

    let (num_str, unit) = input
        .char_indices()
        .find(|(_, c)| c.is_alphabetic())
        .map_or((input.as_str(), ""), |(idx, _)| {
            (&input[..idx], &input[idx..])
        });

    let num_str = num_str.trim();
    if let Ok(val) = num_str.parse::<u64>() {
        let seconds = match unit.trim() {
            "s" | "sec" | "secs" | "second" | "seconds" => val,
            "m" | "min" | "mins" | "minute" | "minutes" => val * 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => val * 3600,
            "d" | "day" | "days" => val * 86400,
            "" => {
                if val > 1_600_000_000 {
                    return Ok(val);
                } else {
                    val * 60
                }
            }
            _ => return Err(format!("Unknown time unit `{unit}` in `{input}`.")),
        };
        return Ok(relative_from + seconds);
    }

    if input == "tomorrow" {
        return Ok(relative_from + 86400);
    }

    Err(format!("Could not parse schedule time `{input}`."))
}

/// Deterministic command-palette interpretation. A leading verb is always an
/// explicit user request; otherwise the entire entry is an ordinary question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteIntent {
    Ask(String),
    See(String),
    Sheet(String),
    Summarize(String),
    Define(String),
    Plan(String),
    Tidy(String),
    File(String),
    Remember(String),
    History(String),
    Schedule(String),
    Remind(String),
}

pub fn parse_palette_intent(input: &str) -> PaletteIntent {
    let full = input.trim().to_owned();
    let mut parts = full.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().unwrap_or_default().trim().to_owned();
    match verb.as_str() {
        "ask" => PaletteIntent::Ask(rest),
        "see" => PaletteIntent::See(rest),
        "sheet" => PaletteIntent::Sheet(rest),
        "summarize" | "summary" => PaletteIntent::Summarize(rest),
        "define" => PaletteIntent::Define(rest),
        "plan" => PaletteIntent::Plan(rest),
        "tidy" => PaletteIntent::Tidy(rest),
        "file" => PaletteIntent::File(rest),
        "remember" => PaletteIntent::Remember(rest),
        "history" => PaletteIntent::History(rest),
        "schedule" => PaletteIntent::Schedule(rest),
        "remind" => PaletteIntent::Remind(rest),
        _ => PaletteIntent::Ask(full),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideControl {
    Back,
    Next,
    Quit,
    DoIt,
}

pub fn guide_control_from_action(action: &str) -> Option<GuideControl> {
    match action {
        "back" => Some(GuideControl::Back),
        "next" => Some(GuideControl::Next),
        "quit" => Some(GuideControl::Quit),
        "do-it" | "do_it" => Some(GuideControl::DoIt),
        _ => None,
    }
}

pub fn apply_preferences(instruction: &str, preamble: Option<&str>) -> String {
    match preamble.map(str::trim).filter(|value| !value.is_empty()) {
        Some(preamble) if !instruction.starts_with(preamble) => {
            format!("{preamble}\n\n{instruction}")
        }
        _ => instruction.into(),
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

/// Model-neutral tool-loop turn. Models with native function calling are
/// normalized into this shape; smaller models emit the same snake_case JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDecision {
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default)]
    pub say_to_user: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub ask_user: Option<String>,
}

impl ToolDecision {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.done || self.ask_user.is_some() {
            return Ok(());
        }
        if self.tool.as_deref().is_none_or(str::is_empty) {
            return Err("A tool decision must select a tool, finish, or ask the user.");
        }
        if !self.args.is_object() {
            return Err("Tool decision args must be a JSON object.");
        }
        Ok(())
    }
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

pub const LOCAL_SUMMARY_CHUNK_TOKENS: usize = 2_500;
pub const LOCAL_SUMMARY_REDUCE_TOKENS: usize = 8_000;

/// Estimate tokens cheaply while retaining headroom for model-specific
/// tokenizers. Source code generally tokenizes more densely than prose.
pub fn estimate_tokens(text: &str) -> usize {
    let characters = text.chars().count();
    if characters == 0 {
        return 0;
    }
    let divisor = if looks_like_code(text) { 3.5 } else { 4.0 };
    (characters as f64 / divisor).ceil() as usize
}

/// A deliberately conservative code heuristic. It needs no language parser
/// and is only used to choose a summary orientation and token headroom.
pub fn looks_like_code(text: &str) -> bool {
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() < 3 {
        return false;
    }
    let signals = lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("def ")
                || trimmed.starts_with("class ")
                || trimmed.starts_with("func ")
                || trimmed.starts_with("impl ")
                || trimmed.starts_with("struct ")
                || trimmed.starts_with("interface ")
                || trimmed.ends_with('{')
                || trimmed.ends_with(';')
                || (line.len() > trimmed.len() && !trimmed.starts_with('-'))
        })
        .count();
    signals * 3 >= lines.len()
}

pub fn selection_summary_instruction(text: &str) -> &'static str {
    if looks_like_code(text) {
        "Give a quick newcomer-oriented recap in no more than 500 characters. State what this file or module is for, then only its two or three most important components, entry points, or dependencies. End naturally after that; skip introductions and line-by-line detail."
    } else {
        "Give a quick recap in no more than 500 characters. State the page or selection's main purpose, then only its two or three most important points. End naturally after that; skip introductions, examples, and minor detail."
    }
}

pub fn chunk_summary_instruction(source_is_code: bool) -> &'static str {
    if source_is_code {
        "Summarize this part of a source file compactly. Preserve its purpose, APIs, control flow, and dependencies so a later pass can accurately summarize the whole file."
    } else {
        "Summarize this part compactly, retaining facts, structure, and conclusions so a later pass can accurately summarize the complete selection."
    }
}

pub fn reduce_summary_instruction(source_is_code: bool) -> &'static str {
    if source_is_code {
        "Combine these partial source-file summaries into one newcomer-oriented overview. State the file's purpose, main components and entry points, important data/control flow, and notable dependencies. Do not describe it line by line."
    } else {
        "Combine these partial summaries into one complete, concise summary. Preserve the selection's overall structure, key facts, and conclusions."
    }
}

/// Split a large selection into complete, ordered text chunks. We prefer blank
/// lines and common top-level declarations, then split only oversized blocks
/// at line or character boundaries so no input is silently discarded.
pub fn structural_summary_chunks(text: &str, target_tokens: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let max_characters = ((target_tokens.max(1) as f64) * 3.5).floor() as usize;
    let blocks = structural_blocks(text);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for block in blocks {
        for fragment in split_at_character_limit(&block, max_characters) {
            if !current.is_empty()
                && current.chars().count() + fragment.chars().count() > max_characters
            {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(&fragment);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Group partial summaries into bounded reduction requests. Repeated calls
/// yield a hierarchical reduction for inputs beyond one context window.
pub fn summary_reduce_batches(summaries: &[String], max_tokens: usize) -> Vec<Vec<String>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_tokens = 0;
    for summary in summaries {
        let tokens = estimate_tokens(summary);
        if !current.is_empty() && current_tokens + tokens > max_tokens {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current_tokens += tokens;
        current.push(summary.clone());
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn structural_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if !current.is_empty() && is_structural_start(line) {
            blocks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        if line.trim().is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn is_structural_start(line: &str) -> bool {
    let line = line.trim_start();
    [
        "fn ",
        "pub fn ",
        "def ",
        "class ",
        "func ",
        "impl ",
        "struct ",
        "interface ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn split_at_character_limit(value: &str, max_characters: usize) -> Vec<String> {
    if value.chars().count() <= max_characters {
        return vec![value.into()];
    }
    let mut fragments = Vec::new();
    let mut remainder = value;
    while remainder.chars().count() > max_characters {
        let split_at = remainder
            .char_indices()
            .nth(max_characters)
            .map(|(index, _)| index)
            .expect("character count was checked above");
        fragments.push(remainder[..split_at].into());
        remainder = &remainder[split_at..];
    }
    if !remainder.is_empty() {
        fragments.push(remainder.into());
    }
    fragments
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
    parse_markdown_table(value).map(|table| table_html_document(&table))
}

/// A rectangular table recovered from clipboard HTML or Markdown. The first
/// row always acts as the header because every downstream representation
/// (Markdown, semantic HTML) requires one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Parse a Markdown table into its cells. Strict by design: a malformed table
/// is left alone rather than guessed at.
pub fn parse_markdown_table(value: &str) -> Option<ExtractedTable> {
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
    Some(ExtractedTable {
        headers: headers.iter().map(|cell| cell.to_string()).collect(),
        rows: rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.to_string()).collect())
            .collect(),
    })
}

fn table_html_document(table: &ExtractedTable) -> String {
    let header_html = table
        .headers
        .iter()
        .map(|cell| format!("<th>{}</th>", html_escape(cell)))
        .collect::<String>();
    let rows_html = table
        .rows
        .iter()
        .map(|row| {
            let cells = row
                .iter()
                .map(|cell| format!("<td>{}</td>", html_escape(cell)))
                .collect::<String>();
            format!("<tr>{cells}</tr>")
        })
        .collect::<String>();
    format!(
        "<html><body><table><thead><tr>{header_html}</tr></thead><tbody>{rows_html}</tbody></table></body></html>"
    )
}

/// Cells joined by tabs paste into spreadsheets as a real grid, which is the
/// plain-text representation most destinations actually want from a table.
pub fn table_tsv(table: &ExtractedTable) -> String {
    let sanitize = |cell: &String| cell.replace(['\t', '\n', '\r'], " ");
    std::iter::once(&table.headers)
        .chain(table.rows.iter())
        .map(|row| row.iter().map(sanitize).collect::<Vec<_>>().join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn table_markdown(table: &ExtractedTable) -> String {
    let cell = |value: &String| value.replace('|', "\\|");
    let line = |row: &Vec<String>| {
        format!(
            "| {} |",
            row.iter().map(cell).collect::<Vec<_>>().join(" | ")
        )
    };
    let separator = format!(
        "|{}|",
        table
            .headers
            .iter()
            .map(|_| " --- ")
            .collect::<Vec<_>>()
            .join("|")
    );
    std::iter::once(line(&table.headers))
        .chain(std::iter::once(separator))
        .chain(table.rows.iter().map(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The smart-copy table representations: a TSV plain text for spreadsheets, a
/// semantic HTML table for rich editors, and a readable Markdown RTF form.
pub fn table_clipboard_contents(table: &ExtractedTable) -> ClipboardContents {
    ClipboardContents {
        text: table_tsv(table),
        html: table_html_document(table),
        rtf: format!(
            "{{\\rtf1\\ansi\\deff0 {} }}",
            rtf_escape(&table_markdown(table))
        ),
    }
}

/// One chunk of scanned HTML: raw text, or a named opening/closing tag.
enum HtmlChunk<'a> {
    Text(&'a str),
    Tag {
        name: String,
        closing: bool,
        attributes: &'a str,
    },
}

/// A deliberately small, quote-aware HTML scanner. Source applications write
/// well-formed table markup even when the surrounding document is noisy, so a
/// full HTML parser dependency is not warranted for the clipboard.
fn html_chunks(html: &str) -> Vec<HtmlChunk<'_>> {
    let mut chunks = Vec::new();
    let bytes = html.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        let Some(open) = html[position..].find('<').map(|at| position + at) else {
            chunks.push(HtmlChunk::Text(&html[position..]));
            break;
        };
        if open > position {
            chunks.push(HtmlChunk::Text(&html[position..open]));
        }
        let rest = &html[open..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            position = comment
                .find("-->")
                .map(|at| open + 4 + at + 3)
                .unwrap_or(html.len());
            continue;
        }
        let mut closing = false;
        let mut name = String::new();
        let mut cursor = open + 1;
        if bytes.get(cursor) == Some(&b'/') {
            closing = true;
            cursor += 1;
        }
        while let Some(&byte) = bytes.get(cursor) {
            if byte.is_ascii_alphanumeric() {
                name.push(byte.to_ascii_lowercase() as char);
                cursor += 1;
            } else {
                break;
            }
        }
        // Skip to the closing '>' while honoring quoted attribute values,
        // which may legally contain '>' characters.
        let attributes_start = cursor;
        let mut quote: Option<u8> = None;
        while let Some(&byte) = bytes.get(cursor) {
            cursor += 1;
            match quote {
                Some(open_quote) if byte == open_quote => quote = None,
                Some(_) => {}
                None if byte == b'"' || byte == b'\'' => quote = Some(byte),
                None if byte == b'>' => break,
                None => {}
            }
        }
        if !name.is_empty() {
            let attributes_end = if bytes.get(cursor.saturating_sub(1)) == Some(&b'>') {
                cursor.saturating_sub(1)
            } else {
                cursor
            };
            chunks.push(HtmlChunk::Tag {
                name,
                closing,
                attributes: &html[attributes_start..attributes_end],
            });
        }
        position = cursor;
    }
    chunks
}

/// Decode the small set of entities that matter for clipboard table cells.
fn decode_html_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        decoded.push_str(&rest[..at]);
        rest = &rest[at..];
        // ';' is ASCII, so a byte-wise search never lands inside a character.
        let Some(end) = rest.bytes().take(12).position(|byte| byte == b';') else {
            decoded.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|digits| digits.parse().ok())
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(character) => {
                decoded.push(character);
                rest = &rest[end + 1..];
            }
            None => {
                decoded.push('&');
                rest = &rest[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

fn normalize_cell_text(value: &str) -> String {
    decode_html_entities(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the first `<table>` in clipboard HTML into clean cells. Nested
/// tables and style/script content are skipped; a table too small to be a
/// grid (a single cell) is rejected so smart copy leaves it alone.
pub fn extract_html_table(html: &str) -> Option<ExtractedTable> {
    let mut collected: Vec<Vec<RawTableCell>> = Vec::new();
    let mut in_table = false;
    let mut nested_tables = 0usize;
    let mut row: Option<Vec<RawTableCell>> = None;
    let mut cell: Option<String> = None;
    let mut cell_colspan = 1usize;
    let mut cell_rowspan = 1usize;
    let mut skip_until: Option<String> = None;
    for chunk in html_chunks(html) {
        match chunk {
            HtmlChunk::Text(text) => {
                if skip_until.is_none()
                    && nested_tables == 0
                    && let Some(cell) = cell.as_mut()
                {
                    cell.push_str(text);
                }
            }
            HtmlChunk::Tag {
                name,
                closing,
                attributes,
            } => {
                if let Some(waiting) = &skip_until {
                    if closing && name == *waiting {
                        skip_until = None;
                    }
                    continue;
                }
                if matches!(name.as_str(), "style" | "script") && !closing {
                    skip_until = Some(name);
                    continue;
                }
                if name == "table" {
                    match (closing, in_table) {
                        (false, false) => in_table = true,
                        (false, true) => nested_tables += 1,
                        (true, true) if nested_tables > 0 => nested_tables -= 1,
                        (true, true) => break,
                        (true, false) => {}
                    }
                    continue;
                }
                if !in_table || nested_tables > 0 {
                    continue;
                }
                match (name.as_str(), closing) {
                    ("tr", false) => {
                        finish_row(
                            &mut collected,
                            &mut row,
                            &mut cell,
                            &mut cell_colspan,
                            &mut cell_rowspan,
                        );
                        row = Some(Vec::new());
                    }
                    ("tr", true) => finish_row(
                        &mut collected,
                        &mut row,
                        &mut cell,
                        &mut cell_colspan,
                        &mut cell_rowspan,
                    ),
                    ("td" | "th", false) => {
                        finish_cell(&mut row, &mut cell, &mut cell_colspan, &mut cell_rowspan);
                        if row.is_none() {
                            row = Some(Vec::new());
                        }
                        cell = Some(String::new());
                        cell_colspan = table_span(attributes, "colspan");
                        cell_rowspan = table_span(attributes, "rowspan");
                    }
                    ("td" | "th", true) => {
                        finish_cell(&mut row, &mut cell, &mut cell_colspan, &mut cell_rowspan)
                    }
                    ("br", false) => {
                        if let Some(cell) = cell.as_mut() {
                            cell.push(' ');
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    finish_row(
        &mut collected,
        &mut row,
        &mut cell,
        &mut cell_colspan,
        &mut cell_rowspan,
    );
    let mut collected = expand_table_spans(collected)?;
    collected.retain(|row| row.iter().any(|cell| !cell.is_empty()));
    let columns = collected.iter().map(Vec::len).max()?;
    if columns == 0 || (columns < 2 && collected.len() < 2) {
        return None;
    }
    for row in &mut collected {
        row.resize(columns, String::new());
    }
    let mut rows = collected.into_iter();
    Some(ExtractedTable {
        headers: rows.next()?,
        rows: rows.collect(),
    })
}

#[derive(Debug, Clone)]
struct RawTableCell {
    text: String,
    colspan: usize,
    rowspan: usize,
}

const MAX_TABLE_SPAN: usize = 64;
const MAX_TABLE_COLUMNS: usize = 512;
const MAX_TABLE_ROWS: usize = 2_048;

fn table_span(attributes: &str, name: &str) -> usize {
    html_attribute(attributes, name)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=MAX_TABLE_SPAN).contains(value))
        .unwrap_or(1)
}

/// Find one exact HTML attribute name without accepting lookalikes such as
/// `data-colspan`. Values can be quoted or bare; callers decide how to use
/// them. This scanner shares the clipboard parser's intentionally small scope.
fn html_attribute<'a>(attributes: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = attributes.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        let name_start = position;
        while bytes
            .get(position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_'))
        {
            position += 1;
        }
        if position == name_start {
            position += 1;
            continue;
        }
        let name = &attributes[name_start..position];
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        if bytes.get(position) != Some(&b'=') {
            continue;
        }
        position += 1;
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        let quote = bytes
            .get(position)
            .copied()
            .filter(|byte| matches!(*byte, b'\'' | b'"'));
        if quote.is_some() {
            position += 1;
        }
        let value_start = position;
        while let Some(byte) = bytes.get(position) {
            if quote.is_some_and(|expected| *byte == expected)
                || quote.is_none() && byte.is_ascii_whitespace()
            {
                break;
            }
            position += 1;
        }
        let value_end = position;
        if quote.is_some() && bytes.get(position).is_some() {
            position += 1;
        }
        if name.eq_ignore_ascii_case(wanted) {
            return Some(&attributes[value_start..value_end]);
        }
    }
    None
}

fn finish_cell(
    row: &mut Option<Vec<RawTableCell>>,
    cell: &mut Option<String>,
    colspan: &mut usize,
    rowspan: &mut usize,
) {
    if let Some(text) = cell.take() {
        row.get_or_insert_with(Vec::new).push(RawTableCell {
            text: normalize_cell_text(&text),
            colspan: *colspan,
            rowspan: *rowspan,
        });
    }
    *colspan = 1;
    *rowspan = 1;
}

fn finish_row(
    collected: &mut Vec<Vec<RawTableCell>>,
    row: &mut Option<Vec<RawTableCell>>,
    cell: &mut Option<String>,
    colspan: &mut usize,
    rowspan: &mut usize,
) {
    finish_cell(row, cell, colspan, rowspan);
    if let Some(cells) = row.take() {
        collected.push(cells);
    }
}

/// Expand merged HTML cells to a rectangular grid. The merged value appears
/// in its top-left position and covered positions become explicit blanks,
/// preserving spreadsheet geometry without inventing duplicate values.
fn expand_table_spans(rows: Vec<Vec<RawTableCell>>) -> Option<Vec<Vec<String>>> {
    let mut expanded = Vec::new();
    let mut pending: Vec<usize> = Vec::new();
    for raw_row in rows {
        if expanded.len() >= MAX_TABLE_ROWS {
            return None;
        }
        let mut row = Vec::new();
        let mut column = 0usize;
        for cell in raw_row {
            consume_pending_table_cells(&mut pending, &mut row, &mut column);
            if column.saturating_add(cell.colspan) > MAX_TABLE_COLUMNS {
                return None;
            }
            row.push(cell.text);
            if pending.len() <= column {
                pending.resize(column + 1, 0);
            }
            pending[column] = cell.rowspan.saturating_sub(1);
            for offset in 1..cell.colspan {
                row.push(String::new());
                if pending.len() <= column + offset {
                    pending.resize(column + offset + 1, 0);
                }
                pending[column + offset] = cell.rowspan.saturating_sub(1);
            }
            column += cell.colspan;
        }
        if pending.len() > MAX_TABLE_COLUMNS {
            return None;
        }
        consume_pending_table_cells(&mut pending, &mut row, &mut column);
        while column < pending.len() {
            if pending[column] > 0 {
                row.push(String::new());
                pending[column] -= 1;
            } else {
                row.push(String::new());
            }
            column += 1;
        }
        expanded.push(row);
    }
    Some(expanded)
}

fn consume_pending_table_cells(pending: &mut [usize], row: &mut Vec<String>, column: &mut usize) {
    while pending.get(*column).copied().unwrap_or_default() > 0 {
        row.push(String::new());
        pending[*column] -= 1;
        *column += 1;
    }
}

/// Keep only structural tags and text so a small local model sees compact
/// semantic HTML instead of a wall of styling attributes.
pub fn simplify_html(html: &str) -> String {
    const KEPT: [&str; 18] = [
        "p",
        "br",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "li",
        "strong",
        "b",
        "em",
        "i",
        "blockquote",
        "code",
        "a",
    ];
    let mut simplified = String::new();
    let mut skip_until: Option<String> = None;
    for chunk in html_chunks(html) {
        match chunk {
            HtmlChunk::Text(text) => {
                if skip_until.is_none() {
                    let normalized = decode_html_entities(text);
                    let mut compact = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !compact.is_empty() {
                        if !simplified.is_empty() && !simplified.ends_with('>') {
                            compact.insert(0, ' ');
                        }
                        simplified.push_str(&compact);
                    }
                }
            }
            HtmlChunk::Tag {
                name,
                closing,
                attributes,
            } => {
                if let Some(waiting) = &skip_until {
                    if closing && name == *waiting {
                        skip_until = None;
                    }
                    continue;
                }
                if matches!(name.as_str(), "style" | "script" | "head") && !closing {
                    skip_until = Some(name);
                    continue;
                }
                if KEPT.contains(&name.as_str()) {
                    simplified.push_str(if closing { "</" } else { "<" });
                    simplified.push_str(&name);
                    if !closing
                        && name == "a"
                        && let Some(href) = html_attribute(attributes, "href")
                    {
                        simplified.push_str(" href=\"");
                        simplified.push_str(href);
                        simplified.push('"');
                    }
                    simplified.push('>');
                }
            }
        }
    }
    simplified
}

/// Detect column-style plain text (tab-separated or space-aligned) that a
/// local model can rebuild into a Markdown table. Code is excluded because
/// indentation runs would otherwise look like columns.
pub fn looks_tabular(text: &str) -> bool {
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() < 2 {
        return false;
    }
    let tabbed = lines.iter().filter(|line| line.contains('\t')).count();
    if tabbed * 10 >= lines.len() * 6 {
        return true;
    }
    if looks_like_code(text) {
        return false;
    }
    let aligned = lines
        .iter()
        .filter(|line| multi_space_runs(line.trim()) >= 2)
        .count();
    aligned * 10 >= lines.len() * 6
}

fn multi_space_runs(line: &str) -> usize {
    let mut runs = 0;
    let mut consecutive = 0;
    for character in line.chars() {
        if character == ' ' {
            consecutive += 1;
        } else {
            if consecutive >= 2 {
                runs += 1;
            }
            consecutive = 0;
        }
    }
    runs
}

/// What smart copy should do with a captured pasteboard. Deterministic table
/// rebuilds come first; a local model is only a fallback, and anything MICE
/// cannot improve is explicitly left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartCopyPlan {
    /// Enriched representations are ready without any model call.
    Ready {
        contents: ClipboardContents,
        notice: &'static str,
    },
    /// Column-like text needs a local model to rebuild it as a Markdown table.
    ModelMarkdownTable { source: String },
    /// Rich text needs a local model to clean it into Markdown.
    ModelMarkdownClean { source: String },
    /// The pasteboard is left exactly as the user's Cmd-C wrote it.
    NothingToEnrich { reason: &'static str },
}

pub const SMART_COPY_TABLE_NOTICE: &str =
    "Table cleaned: paste into a spreadsheet for a real grid, or a rich editor for a clean table.";

pub fn smart_copy_plan(text: Option<&str>, html: Option<&str>) -> SmartCopyPlan {
    let text = text.map(str::trim).filter(|value| !value.is_empty());
    let html = html.map(str::trim).filter(|value| !value.is_empty());
    if let Some(html) = html {
        // The deterministic table representation currently has plain cell
        // values only. Do not silently turn anchors into unlinked text; leave
        // the source clipboard intact until table-link representations exist.
        if html_table_contains_links(html) {
            return SmartCopyPlan::NothingToEnrich {
                reason: "This table contains links MICE cannot preserve losslessly; it was left as copied.",
            };
        }
        if let Some(table) = extract_html_table(html) {
            return SmartCopyPlan::Ready {
                contents: table_clipboard_contents(&table),
                notice: SMART_COPY_TABLE_NOTICE,
            };
        }
    }
    if let Some(text) = text {
        if let Some(table) = parse_markdown_table(text) {
            return SmartCopyPlan::Ready {
                contents: table_clipboard_contents(&table),
                notice: SMART_COPY_TABLE_NOTICE,
            };
        }
        if looks_tabular(text) {
            return SmartCopyPlan::ModelMarkdownTable {
                source: text.into(),
            };
        }
    }
    if let Some(html) = html {
        let simplified = simplify_html(html);
        if !simplified.trim().is_empty() {
            return SmartCopyPlan::ModelMarkdownClean { source: simplified };
        }
    }
    if text.is_some() {
        return SmartCopyPlan::NothingToEnrich {
            reason: "Plain text is already clean; the clipboard was left as copied.",
        };
    }
    SmartCopyPlan::NothingToEnrich {
        reason: "No text or table on the clipboard to enrich; it was left as copied.",
    }
}

fn html_table_contains_links(html: &str) -> bool {
    let mut table_depth = 0usize;
    for chunk in html_chunks(html) {
        let HtmlChunk::Tag { name, closing, .. } = chunk else {
            continue;
        };
        match (name.as_str(), closing) {
            ("table", false) => table_depth += 1,
            ("table", true) => table_depth = table_depth.saturating_sub(1),
            ("a", false) if table_depth > 0 => return true,
            _ => {}
        }
    }
    false
}

pub fn smart_copy_table_instruction() -> &'static str {
    "Convert the content into one GitHub-flavored Markdown table. Output only the table: a header row, a separator row, then every data row. Preserve every value exactly; do not add, drop, or reorder anything, and write no commentary."
}

pub fn smart_copy_clean_instruction() -> &'static str {
    "Rewrite the content as clean Markdown. Preserve the exact wording, headings, lists, links, and emphasis; remove styling noise. Output only the Markdown with no commentary."
}

/// Split a rich clipboard source only at semantic boundaries. Returning None
/// means a lossless local rewrite cannot be attempted safely, so the caller
/// must leave the user's clipboard untouched rather than truncating it.
pub fn smart_copy_chunks(source: &str, maximum_characters: usize) -> Option<Vec<String>> {
    if source.chars().count() <= maximum_characters {
        return Some(vec![source.into()]);
    }
    let mut boundaries = source
        .match_indices('\n')
        .map(|(index, _)| index + 1)
        .collect::<Vec<_>>();
    for closing in [
        "</p>", "</li>", "</h1>", "</h2>", "</h3>", "</h4>", "</h5>", "</h6>", "<br>",
    ] {
        boundaries.extend(
            source
                .match_indices(closing)
                .map(|(index, _)| index + closing.len()),
        );
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries.retain(|boundary| *boundary < source.len());
    if boundaries.is_empty() {
        return None;
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let mut chosen = None;
        for boundary in boundaries
            .iter()
            .copied()
            .filter(|boundary| *boundary > start)
        {
            if source[start..boundary].chars().count() <= maximum_characters {
                chosen = Some(boundary);
            } else {
                break;
            }
        }
        let end = chosen.unwrap_or(source.len());
        if source[start..end].chars().count() > maximum_characters {
            return None;
        }
        chunks.push(source[start..end].into());
        start = end;
    }
    Some(chunks)
}

/// Normalize visible words for an intentionally strict post-model check. HTML
/// tags/attributes are excluded; Markdown punctuation is excluded. A model
/// that drops, invents, or reorders visible words fails this comparison.
pub fn smart_copy_visible_tokens(value: &str) -> Vec<String> {
    let mut visible = String::new();
    let has_html = value.contains('<') && value.contains('>');
    if has_html {
        let mut skip_until: Option<String> = None;
        for chunk in html_chunks(value) {
            match chunk {
                HtmlChunk::Text(text) if skip_until.is_none() => {
                    visible.push_str(&decode_html_entities(text));
                    visible.push(' ');
                }
                HtmlChunk::Tag { name, closing, .. } => {
                    if let Some(waiting) = &skip_until {
                        if closing && name == *waiting {
                            skip_until = None;
                        }
                    } else if !closing && matches!(name.as_str(), "style" | "script" | "head") {
                        skip_until = Some(name);
                    }
                }
                _ => {}
            }
        }
    } else {
        visible.push_str(value);
    }
    visible
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

pub fn smart_copy_preserves_visible_text(source: &str, result: &str) -> bool {
    smart_copy_visible_tokens(source) == smart_copy_visible_tokens(result)
}

/// Link destinations are meaningful rich-text content. A clean-Markdown model
/// response must retain every source link, in order, rather than merely its
/// visible anchor text.
pub fn smart_copy_preserves_links(source: &str, result: &str) -> bool {
    smart_copy_link_targets(source) == smart_copy_link_targets(result)
}

fn smart_copy_link_targets(value: &str) -> Vec<String> {
    let html_links = html_chunks(value)
        .into_iter()
        .filter_map(|chunk| match chunk {
            HtmlChunk::Tag {
                name,
                closing: false,
                attributes,
            } if name == "a" => html_attribute(attributes, "href").map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !html_links.is_empty() {
        return html_links;
    }
    let mut links = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find("](") {
        let destination = &rest[open + 2..];
        let Some(close) = destination.find(')') else {
            break;
        };
        let link = destination[..close].trim().trim_matches('<');
        let link = link.trim_end_matches('>');
        if !link.is_empty() {
            links.push(link.into());
        }
        rest = &destination[close + 1..];
    }
    links
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

/// RTF is an ASCII format: control characters and every non-ASCII character
/// must be escaped, or accented text and emoji corrupt the rich-text paste.
/// `\uN?` carries a signed 16-bit UTF-16 unit with `?` as the fallback glyph;
/// characters beyond the BMP emit their surrogate pair as two escapes.
fn rtf_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '{' => escaped.push_str("\\{"),
            '}' => escaped.push_str("\\}"),
            '\n' => escaped.push_str("\\line\n"),
            character if (character as u32) < 128 => escaped.push(character),
            character => {
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    escaped.push_str(&format!("\\u{}?", *unit as i16));
                }
            }
        }
    }
    escaped
}

// --- M9 `mice tidy` / M10 `mice file` portable logic -----------------------
//
// Everything here is side-effect-free: the CLI owns walking, hashing,
// Spotlight, model calls, and the actual renames. This module only decides.

/// Destination folders `mice tidy` may propose. `Other` is never a move
/// target: an unrecognized file stays where the user put it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TidyCategory {
    Documents,
    Images,
    Video,
    Audio,
    Archives,
    Code,
    Data,
    Installers,
    Other,
}

impl TidyCategory {
    pub fn folder_name(self) -> &'static str {
        match self {
            Self::Documents => "Documents",
            Self::Images => "Images",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archives => "Archives",
            Self::Code => "Code",
            Self::Data => "Data",
            Self::Installers => "Installers",
            Self::Other => "Other",
        }
    }
}

pub fn categorize_file_name(name: &str) -> TidyCategory {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "pdf" | "doc" | "docx" | "pages" | "txt" | "md" | "rtf" | "odt" | "key" | "ppt"
        | "pptx" | "xls" | "xlsx" | "numbers" | "epub" => TidyCategory::Documents,
        "png" | "jpg" | "jpeg" | "gif" | "heic" | "heif" | "webp" | "tiff" | "bmp" | "svg" => {
            TidyCategory::Images
        }
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => TidyCategory::Video,
        "mp3" | "wav" | "aac" | "flac" | "m4a" | "ogg" | "aiff" => TidyCategory::Audio,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => TidyCategory::Archives,
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "swift" | "c" | "cpp" | "h" | "hpp"
        | "java" | "go" | "rb" | "php" | "sh" | "html" | "css" | "toml" | "yaml" | "yml"
        | "sql" => TidyCategory::Code,
        "json" | "csv" | "tsv" | "xml" | "db" | "sqlite" | "parquet" | "log" | "plist" => {
            TidyCategory::Data
        }
        "dmg" | "pkg" | "ipa" => TidyCategory::Installers,
        _ => TidyCategory::Other,
    }
}

/// One scanned file. Timestamps are epoch seconds; `last_used_ts` comes from
/// Spotlight when available and `content_key` is set only when the CLI hashed
/// the file for duplicate detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TidyFile {
    pub relative_path: String,
    pub size: u64,
    pub modified_ts: Option<u64>,
    pub created_ts: Option<u64>,
    pub last_used_ts: Option<u64>,
    pub content_key: Option<String>,
}

impl TidyFile {
    pub fn file_name(&self) -> &str {
        self.relative_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.relative_path)
    }

    /// The most recent evidence the user touched this file at all.
    pub fn last_activity_ts(&self) -> Option<u64> {
        [self.last_used_ts, self.modified_ts, self.created_ts]
            .into_iter()
            .flatten()
            .max()
    }
}

/// Indices of files sharing identical content, one set per duplicate group.
/// Each set is ordered with the canonical copy (shortest, then lexicographic
/// path) first so proposal logic keeps exactly one.
pub fn duplicate_sets(files: &[TidyFile]) -> Vec<Vec<usize>> {
    let mut by_key = std::collections::BTreeMap::<&str, Vec<usize>>::new();
    for (index, file) in files.iter().enumerate() {
        if let Some(key) = file.content_key.as_deref() {
            by_key.entry(key).or_default().push(index);
        }
    }
    let mut sets = by_key
        .into_values()
        .filter(|set| set.len() > 1)
        .collect::<Vec<_>>();
    for set in &mut sets {
        set.sort_by(|left, right| {
            let left = &files[*left].relative_path;
            let right = &files[*right].relative_path;
            left.len().cmp(&right.len()).then_with(|| left.cmp(right))
        });
    }
    sets
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TidyAction {
    Keep,
    Move,
    TrashCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TidyProposal {
    pub file_index: usize,
    pub action: TidyAction,
    pub category: TidyCategory,
    pub reason: String,
}

pub const TIDY_STALE_SECONDS: u64 = 183 * 24 * 60 * 60;

/// Deterministic pass over a scan: keep one copy per duplicate set, flag
/// long-unused files, and offer category moves only for loose top-level
/// files. Nothing here deletes; a trash candidate is a suggestion the user
/// must individually confirm in the review screen.
pub fn propose_tidy_actions(files: &[TidyFile], stale_cutoff_ts: u64) -> Vec<TidyProposal> {
    let mut duplicate_of = std::collections::BTreeMap::<usize, usize>::new();
    for set in duplicate_sets(files) {
        let canonical = set[0];
        for index in &set[1..] {
            duplicate_of.insert(*index, canonical);
        }
    }
    files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let category = categorize_file_name(file.file_name());
            if let Some(canonical) = duplicate_of.get(&index) {
                return TidyProposal {
                    file_index: index,
                    action: TidyAction::TrashCandidate,
                    category,
                    reason: format!("duplicate of {}", files[*canonical].relative_path),
                };
            }
            let stale = file
                .last_activity_ts()
                .is_some_and(|activity| activity < stale_cutoff_ts);
            if stale {
                return TidyProposal {
                    file_index: index,
                    action: TidyAction::TrashCandidate,
                    category,
                    reason: "not opened in over 6 months".into(),
                };
            }
            let top_level = !file.relative_path.contains('/');
            if top_level && category != TidyCategory::Other {
                return TidyProposal {
                    file_index: index,
                    action: TidyAction::Move,
                    category,
                    reason: format!("file into {}/", category.folder_name()),
                };
            }
            TidyProposal {
                file_index: index,
                action: TidyAction::Keep,
                category,
                reason: String::new(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TidyReport {
    pub total_files: usize,
    pub total_bytes: u64,
    pub stale_files: usize,
    pub stale_bytes: u64,
    pub duplicate_sets: usize,
    pub duplicate_extra_bytes: u64,
}

pub fn tidy_report(files: &[TidyFile], stale_cutoff_ts: u64) -> TidyReport {
    let mut report = TidyReport {
        total_files: files.len(),
        ..TidyReport::default()
    };
    for file in files {
        report.total_bytes += file.size;
        if file
            .last_activity_ts()
            .is_some_and(|activity| activity < stale_cutoff_ts)
        {
            report.stale_files += 1;
            report.stale_bytes += file.size;
        }
    }
    for set in duplicate_sets(files) {
        report.duplicate_sets += 1;
        report.duplicate_extra_bytes +=
            set[1..].iter().map(|index| files[*index].size).sum::<u64>();
    }
    report
}

impl TidyReport {
    pub fn headline(&self) -> String {
        format!(
            "{} files ({}); {} unopened >6 months ({}); {} duplicate sets ({} reclaimable)",
            self.total_files,
            format_bytes(self.total_bytes),
            self.stale_files,
            format_bytes(self.stale_bytes),
            self.duplicate_sets,
            format_bytes(self.duplicate_extra_bytes),
        )
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Parse Spotlight's `mdls -raw` date form, e.g. `2026-07-01 10:00:00 +0000`,
/// into epoch seconds. `(null)` and unindexed files yield None so callers
/// fall back to filesystem timestamps.
pub fn parse_spotlight_date(value: &str) -> Option<u64> {
    let value = value.trim();
    let mut parts = value.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let offset = parts.next().unwrap_or("+0000");
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.split('.').next()?.parse().ok()?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..61).contains(&second) {
        return None;
    }
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let offset_digits = &offset[1..];
    if offset_digits.len() != 4 || !offset_digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let offset_seconds = sign
        * (offset_digits[..2].parse::<i64>().ok()? * 3600
            + offset_digits[2..].parse::<i64>().ok()? * 60);
    let epoch = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
        - offset_seconds;
    u64::try_from(epoch).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// civil-days algorithm), avoiding a date-time dependency.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// One reversible filesystem action applied by `mice tidy` or `mice file`.
/// Paths are absolute; `to` is where the file actually landed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UndoKind {
    Move,
    Trash,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoAction {
    pub kind: UndoKind,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoRun {
    pub id: String,
    pub ts: u64,
    pub tool: String,
    pub actions: Vec<UndoAction>,
}

/// The rename sequence that reverses a run: strict LIFO order, each entry
/// `(current location, original location)`.
pub fn undo_plan(run: &UndoRun) -> Vec<(String, String)> {
    run.actions
        .iter()
        .rev()
        .map(|action| (action.to.clone(), action.from.clone()))
        .collect()
}

pub fn tidy_label_instruction() -> &'static str {
    "In at most eight words, say what this file appears to be, for a tidy-up review list. Answer with only the label itself: no punctuation at the end and no commentary."
}

// --- M10 `mice file` destination ranking ------------------------------------

/// A candidate destination folder from the filing index. `path` is absolute;
/// `description` is the folder name plus an optional cached one-line local
/// model description.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilingCandidate {
    pub path: String,
    pub description: String,
}

pub fn filing_rank_instruction() -> &'static str {
    "Choose the best destination folders for the file described below. Answer with only a JSON object of the form {\"ranking\": [n, n, n]} listing up to three candidate numbers from the provided list, best first. Use only numbers from the list and output nothing else."
}

pub fn filing_prompt(file_summary: &str, candidates: &[FilingCandidate]) -> String {
    let listing = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            format!(
                "{}. {} — {}",
                index + 1,
                candidate.path,
                candidate.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("File:\n{file_summary}\n\nCandidate destinations:\n{listing}")
}

/// Strictly validate a model ranking: a JSON `{"ranking": [...]}` (or bare
/// array) of in-range 1-based candidate numbers. Anything else returns None
/// so the caller falls back to the deterministic ranking — model output never
/// names a path directly.
pub fn parse_filing_ranking(response: &str, candidate_count: usize) -> Option<Vec<usize>> {
    let response = response.trim();
    let start = response.find(['{', '['])?;
    let value: serde_json::Value = serde_json::from_str(&response[start..]).ok()?;
    let entries = value
        .as_array()
        .or_else(|| value.get("ranking").and_then(serde_json::Value::as_array))?;
    let mut ranking = Vec::new();
    for entry in entries {
        let number = usize::try_from(entry.as_u64()?).ok()?;
        if number == 0 || number > candidate_count {
            return None;
        }
        let index = number - 1;
        if !ranking.contains(&index) {
            ranking.push(index);
        }
    }
    if ranking.is_empty() {
        return None;
    }
    ranking.truncate(3);
    Some(ranking)
}

/// Deterministic fallback ranking: score candidates by how many of the file
/// name's tokens appear in the candidate path/description. Ties prefer the
/// shorter (more specific root-level) path. Returns up to three indices.
pub fn rank_candidates_by_name(file_name: &str, candidates: &[FilingCandidate]) -> Vec<usize> {
    let tokens = name_tokens(file_name);
    let mut scored = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let haystack =
                format!("{} {}", candidate.path, candidate.description).to_ascii_lowercase();
            let score = tokens
                .iter()
                .filter(|token| haystack.contains(token.as_str()))
                .count();
            (index, score, candidate.path.len())
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    scored
        .into_iter()
        .take(3)
        .map(|(index, _, _)| index)
        .collect()
}

fn name_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect()
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
        assert_eq!(config.gesture.smart_copy_trigger, "ctrl+alt+c");
    }

    #[test]
    fn unsupported_smart_copy_trigger_is_rejected() {
        let mut config = Config::default();
        config.gesture.smart_copy_trigger = "shift+drag".into();
        assert!(matches!(
            validate_config(&config),
            Err(ConfigError::InvalidSmartCopyTrigger(trigger)) if trigger == "shift+drag"
        ));
    }

    #[test]
    fn rtf_escapes_accents_and_emoji_as_utf16_units() {
        let contents = clipboard_contents("café 😀");
        assert!(contents.rtf.contains("caf\\u233?"));
        // U+1F600 encodes as the UTF-16 surrogate pair D83D DE00.
        assert!(contents.rtf.contains("\\u-10179?\\u-8704?"));
        let ascii = clipboard_contents("plain text");
        assert!(ascii.rtf.contains("plain text"));
    }

    #[test]
    fn config_warnings_flag_unknown_models_triggers_and_ranges() {
        assert!(config_warnings(&Config::default()).is_empty());
        let mut config = Config {
            local_model: "not-a-model".into(),
            ..Config::default()
        };
        config.gesture.trigger = "ctrl+alt+q".into();
        config.gesture.chord_window_ms = 5;
        config.routing.quota_bias_percent = 250;
        let warnings = config_warnings(&config);
        assert_eq!(warnings.len(), 4);
        assert!(warnings[0].contains("not-a-model"));
        assert!(warnings[1].contains("ctrl+alt+q"));
        assert!(warnings[2].contains("chord_window_ms"));
        assert!(warnings[3].contains("quota_bias_percent"));
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
    fn goal_session_allows_review_cancellation_but_not_active_guide_cancellation() {
        let mut session = GoalSession::new();
        assert!(session.cancel().is_ok());
        assert!(matches!(session.state(), GoalState::Cancelled));

        let mut accepted = GoalSession::new();
        accepted.submit_goal("Make a budget".into()).unwrap();
        accepted.review("1. Open Numbers".into()).unwrap();
        accepted.accept().unwrap();
        assert!(accepted.cancel().is_err());
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
    fn tool_decision_requires_a_tool_or_terminal_state() {
        let decision: ToolDecision = serde_json::from_str(
            r#"{"tool":"git.status","args":{},"say_to_user":"Checking status","done":false,"ask_user":null}"#,
        )
        .unwrap();
        assert!(decision.validate().is_ok());
        assert!(
            ToolDecision {
                tool: None,
                args: serde_json::json!({}),
                say_to_user: String::new(),
                done: false,
                ask_user: None,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn light_profile_never_uses_the_local_loop_lane() {
        let lane = route_execution_lane(
            MachineProfile::Light,
            &RoutingConfig::default(),
            false,
            true,
            Some(90),
        );
        assert_eq!(lane, ExecutionLane::Unavailable);
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

    #[test]
    fn code_uses_a_smaller_token_divisor_and_code_summary_orientation() {
        let source = "pub fn run() {\n    let answer = 42;\n    println!(\"{answer}\");\n}\n";
        assert!(looks_like_code(source));
        assert!(estimate_tokens(source) > 10);
        assert!(selection_summary_instruction(source).contains("newcomer"));
    }

    #[test]
    fn prose_uses_the_general_summary_orientation() {
        let prose =
            "MICE helps people understand what is on screen and complete tasks one step at a time.";
        assert!(!looks_like_code(prose));
        assert_eq!(
            selection_summary_instruction(prose),
            "Give a quick recap in no more than 500 characters. State the page or selection's main purpose, then only its two or three most important points. End naturally after that; skip introductions, examples, and minor detail."
        );
    }

    #[test]
    fn chunks_prefer_declaration_boundaries_without_losing_text() {
        let text = "fn first() {}\n\nfn second() {}\n\nfn third() {}\n";
        let chunks = structural_summary_chunks(text, 5);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks.concat(), text);
        assert!(chunks[0].contains("first"));
    }

    #[test]
    fn chunks_split_an_oversized_line_without_truncation() {
        let text = "😀".repeat(40);
        let chunks = structural_summary_chunks(&text, 5);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.is_char_boundary(chunk.len()))
        );
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn html_table_extracts_cells_through_attributes_entities_and_nesting() {
        let html = concat!(
            "<html><head><style>td { color: red; }</style></head><body>",
            "<table class=\"grid\" data-note=\"a > b\">",
            "<thead><tr><th><b>Name</b></th><th>Score &amp; Rank</th></tr></thead>",
            "<tbody><tr><td><span style=\"x\">Ada</span></td><td>10<br>of 12</td></tr>",
            "<tr><td>Grace&nbsp;H</td><td>9</td></tr></tbody>",
            "</table></body></html>"
        );
        let table = extract_html_table(html).unwrap();
        assert_eq!(table.headers, vec!["Name", "Score & Rank"]);
        assert_eq!(
            table.rows,
            vec![vec!["Ada", "10 of 12"], vec!["Grace H", "9"]]
        );
    }

    #[test]
    fn html_table_skips_nested_tables_and_rejects_a_single_cell() {
        let nested = "<table><tr><td>Outer<table><tr><td>Inner</td></tr></table></td><td>Two</td></tr><tr><td>A</td><td>B</td></tr></table>";
        let table = extract_html_table(nested).unwrap();
        assert_eq!(table.headers, vec!["Outer", "Two"]);
        assert_eq!(table.rows, vec![vec!["A", "B"]]);
        assert_eq!(
            extract_html_table("<table><tr><td>Lonely</td></tr></table>"),
            None
        );
        assert_eq!(extract_html_table("<p>No table here</p>"), None);
    }

    #[test]
    fn table_contents_pair_tsv_text_with_semantic_html() {
        let table = ExtractedTable {
            headers: vec!["Name".into(), "Score".into()],
            rows: vec![vec!["Ada".into(), "10".into()]],
        };
        let contents = table_clipboard_contents(&table);
        assert_eq!(contents.text, "Name\tScore\nAda\t10");
        assert!(
            contents
                .html
                .contains("<thead><tr><th>Name</th><th>Score</th></tr></thead>")
        );
        assert!(contents.rtf.contains("| Ada | 10 |"));
    }

    #[test]
    fn tabular_text_is_detected_without_flagging_prose_or_code() {
        assert!(looks_tabular("Name\tScore\nAda\t10\nGrace\t9"));
        assert!(looks_tabular(
            "Name    Score\nAda     10      yes\nGrace   9       no"
        ));
        assert!(!looks_tabular("A single line\twith a tab"));
        assert!(!looks_tabular(
            "MICE helps people understand what is on screen.\nIt acts one step at a time."
        ));
        assert!(!looks_tabular(
            "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}"
        ));
    }

    #[test]
    fn smart_copy_prefers_deterministic_table_rebuilds() {
        let plan = smart_copy_plan(
            Some("Name Score"),
            Some(
                "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Ada</td><td>10</td></tr></table>",
            ),
        );
        let SmartCopyPlan::Ready { contents, notice } = plan else {
            panic!("expected a deterministic table plan");
        };
        assert_eq!(contents.text, "Name\tScore\nAda\t10");
        assert_eq!(notice, SMART_COPY_TABLE_NOTICE);

        let markdown_plan =
            smart_copy_plan(Some("| Name | Score |\n| --- | --- |\n| Ada | 10 |"), None);
        let SmartCopyPlan::Ready { contents, .. } = markdown_plan else {
            panic!("expected the markdown table to convert without a model");
        };
        assert_eq!(contents.text, "Name\tScore\nAda\t10");
    }

    #[test]
    fn smart_copy_leaves_linked_html_tables_untouched() {
        assert!(matches!(
            smart_copy_plan(
                None,
                Some("<table><tr><td><a href=\"https://example.test\">Docs</a></td></tr></table>"),
            ),
            SmartCopyPlan::NothingToEnrich { .. }
        ));
    }

    #[test]
    fn smart_copy_expands_colspan_and_rowspan_without_losing_geometry() {
        let table = extract_html_table(
            "<table><tr><th colspan=\"2\">Name</th><th>Score</th></tr>\
             <tr><td rowspan=\"2\">Ada</td><td>Math</td><td>10</td></tr>\
             <tr><td>Science</td><td>9</td></tr></table>",
        )
        .expect("a valid table");
        assert_eq!(
            table.headers,
            vec!["Name".to_owned(), String::new(), "Score".to_owned()]
        );
        assert_eq!(
            table.rows,
            vec![
                vec!["Ada".to_owned(), "Math".to_owned(), "10".to_owned()],
                vec![String::new(), "Science".to_owned(), "9".to_owned()],
            ]
        );
        assert_eq!(
            table_tsv(&table),
            "Name\t\tScore\nAda\tMath\t10\n\tScience\t9"
        );
    }

    #[test]
    fn smart_copy_falls_back_to_the_model_lanes_or_leaves_the_clipboard_alone() {
        assert_eq!(
            smart_copy_plan(Some("Name\tScore\nAda\t10"), None),
            SmartCopyPlan::ModelMarkdownTable {
                source: "Name\tScore\nAda\t10".into(),
            }
        );
        let clean = smart_copy_plan(
            Some("Title Body"),
            Some(
                "<div style=\"font:12px\"><h1>Title</h1><script>alert(1)</script><p>Body &amp; more</p></div>",
            ),
        );
        assert_eq!(
            clean,
            SmartCopyPlan::ModelMarkdownClean {
                source: "<h1>Title</h1><p>Body & more</p>".into(),
            }
        );
        assert!(matches!(
            smart_copy_plan(Some("Just a plain sentence."), None),
            SmartCopyPlan::NothingToEnrich { .. }
        ));
        assert!(matches!(
            smart_copy_plan(None, None),
            SmartCopyPlan::NothingToEnrich { .. }
        ));
    }

    #[test]
    fn smart_copy_chunks_only_at_lossless_boundaries() {
        assert_eq!(
            smart_copy_chunks("One\nTwo\nThree", 7),
            Some(vec!["One\n".into(), "Two\n".into(), "Three".into()])
        );
        assert_eq!(smart_copy_chunks("a-very-long-token", 4), None);
    }

    #[test]
    fn smart_copy_visible_text_check_rejects_dropped_or_reordered_words() {
        assert!(smart_copy_preserves_visible_text(
            "<h1>Title</h1><p>Body &amp; more</p>",
            "# Title\n\nBody & more"
        ));
        assert!(!smart_copy_preserves_visible_text("Title body", "Title"));
        assert!(!smart_copy_preserves_visible_text(
            "Title body",
            "Body title"
        ));
    }

    #[test]
    fn smart_copy_retains_link_destinations() {
        let source = simplify_html(
            "<p>Read <a class=\"cta\" href=\"https://example.test/docs\">the docs</a>.</p>",
        );
        assert_eq!(
            source,
            "<p>Read<a href=\"https://example.test/docs\">the docs</a>.</p>"
        );
        assert!(smart_copy_preserves_links(
            &source,
            "Read [the docs](https://example.test/docs)."
        ));
        assert!(!smart_copy_preserves_links(&source, "Read the docs."));
    }

    #[test]
    fn smart_copy_rejects_lookalike_or_unbounded_table_spans() {
        assert_eq!(table_span(" data-colspan=999", "colspan"), 1);
        assert_eq!(table_span(" colspan=999999", "colspan"), 1);
        assert_eq!(table_span(" colspan='3'", "colspan"), 3);
        assert!(extract_html_table(
            "<table><tr><td colspan=999999>bad</td><td>x</td></tr><tr><td>a</td><td>b</td></tr></table>"
        )
        .is_some());
    }

    fn tidy_file(path: &str, size: u64, modified: u64, key: Option<&str>) -> TidyFile {
        TidyFile {
            relative_path: path.into(),
            size,
            modified_ts: Some(modified),
            created_ts: Some(modified),
            last_used_ts: None,
            content_key: key.map(str::to_owned),
        }
    }

    #[test]
    fn tidy_proposals_keep_one_duplicate_flag_stale_and_move_loose_files() {
        let cutoff = 1_000;
        let files = vec![
            tidy_file("report.pdf", 10, 2_000, Some("k1")),
            tidy_file("nested/report copy.pdf", 10, 2_000, Some("k1")),
            tidy_file("old-notes.txt", 5, 100, None),
            tidy_file("photo.heic", 7, 2_000, None),
            tidy_file("mystery.xyz", 3, 2_000, None),
            tidy_file("projects/keep.rs", 2, 2_000, None),
        ];
        let proposals = propose_tidy_actions(&files, cutoff);
        assert_eq!(proposals[0].action, TidyAction::Move);
        assert_eq!(proposals[0].category, TidyCategory::Documents);
        assert_eq!(proposals[1].action, TidyAction::TrashCandidate);
        assert!(proposals[1].reason.contains("duplicate of report.pdf"));
        assert_eq!(proposals[2].action, TidyAction::TrashCandidate);
        assert!(proposals[2].reason.contains("6 months"));
        assert_eq!(proposals[3].action, TidyAction::Move);
        assert_eq!(proposals[3].category, TidyCategory::Images);
        assert_eq!(proposals[4].action, TidyAction::Keep);
        assert_eq!(proposals[5].action, TidyAction::Keep);
    }

    #[test]
    fn spotlight_last_used_rescues_an_old_modified_file_from_staleness() {
        let mut file = tidy_file("old.pdf", 1, 100, None);
        assert!(
            propose_tidy_actions(std::slice::from_ref(&file), 1_000)[0].action
                == TidyAction::TrashCandidate
        );
        file.last_used_ts = Some(5_000);
        assert_eq!(
            propose_tidy_actions(std::slice::from_ref(&file), 1_000)[0].action,
            TidyAction::Move
        );
    }

    #[test]
    fn tidy_report_counts_bytes_stale_files_and_reclaimable_duplicates() {
        let files = vec![
            tidy_file("a.pdf", 1_000, 2_000, Some("k")),
            tidy_file("b.pdf", 1_000, 2_000, Some("k")),
            tidy_file("stale.log", 500, 10, None),
        ];
        let report = tidy_report(&files, 1_000);
        assert_eq!(report.total_files, 3);
        assert_eq!(report.total_bytes, 2_500);
        assert_eq!(report.stale_files, 1);
        assert_eq!(report.stale_bytes, 500);
        assert_eq!(report.duplicate_sets, 1);
        assert_eq!(report.duplicate_extra_bytes, 1_000);
        assert!(report.headline().contains("3 files"));
        assert_eq!(format_bytes(3_435_973_837), "3.2 GB");
    }

    #[test]
    fn spotlight_dates_parse_with_offsets_and_reject_garbage() {
        assert_eq!(
            parse_spotlight_date("1970-01-02 00:00:00 +0000"),
            Some(86_400)
        );
        assert_eq!(
            parse_spotlight_date("2026-07-01 10:00:00 +0000"),
            Some(1_782_900_000)
        );
        assert_eq!(
            parse_spotlight_date("2026-07-01 12:00:00 +0200"),
            parse_spotlight_date("2026-07-01 10:00:00 +0000")
        );
        assert_eq!(parse_spotlight_date("(null)"), None);
        assert_eq!(parse_spotlight_date(""), None);
        assert_eq!(parse_spotlight_date("not a date"), None);
    }

    #[test]
    fn undo_plans_reverse_actions_in_lifo_order() {
        let run = UndoRun {
            id: "run-1".into(),
            ts: 1,
            tool: "tidy".into(),
            actions: vec![
                UndoAction {
                    kind: UndoKind::Move,
                    from: "/d/a.pdf".into(),
                    to: "/d/Documents/a.pdf".into(),
                },
                UndoAction {
                    kind: UndoKind::Trash,
                    from: "/d/b.pdf".into(),
                    to: "/trash/b.pdf".into(),
                },
            ],
        };
        assert_eq!(
            undo_plan(&run),
            vec![
                ("/trash/b.pdf".into(), "/d/b.pdf".into()),
                ("/d/Documents/a.pdf".into(), "/d/a.pdf".into()),
            ]
        );
    }

    #[test]
    fn filing_rankings_validate_strictly_and_fall_back_deterministically() {
        let candidates = vec![
            FilingCandidate {
                path: "/github/mice".into(),
                description: "Rust desktop assistant".into(),
            },
            FilingCandidate {
                path: "/github/taxes-2026".into(),
                description: "tax documents".into(),
            },
            FilingCandidate {
                path: "/github/website".into(),
                description: "personal site".into(),
            },
        ];
        assert_eq!(
            parse_filing_ranking("{\"ranking\": [2, 1]}", candidates.len()),
            Some(vec![1, 0])
        );
        assert_eq!(
            parse_filing_ranking("Sure! {\"ranking\": [2]}", candidates.len()),
            Some(vec![1])
        );
        assert_eq!(
            parse_filing_ranking("{\"ranking\": [7]}", candidates.len()),
            None
        );
        assert_eq!(
            parse_filing_ranking("{\"ranking\": [0]}", candidates.len()),
            None
        );
        assert_eq!(
            parse_filing_ranking("/github/taxes-2026", candidates.len()),
            None
        );
        let ranked = rank_candidates_by_name("tax-return-2026.pdf", &candidates);
        assert_eq!(ranked[0], 1);
    }

    #[test]
    fn reduce_batches_remain_bounded_and_ordered() {
        let summaries = vec!["one ".repeat(400), "two ".repeat(400), "three ".repeat(400)];
        let batches = summary_reduce_batches(&summaries, 900);
        assert!(batches.len() >= 2);
        assert_eq!(batches.concat(), summaries);
    }

    #[test]
    fn palette_verbs_are_explicit_and_preferences_are_once_only() {
        assert_eq!(
            parse_palette_intent("define entropy"),
            PaletteIntent::Define("entropy".into())
        );
        assert_eq!(
            parse_palette_intent("ask define a trait"),
            PaletteIntent::Ask("define a trait".into())
        );
        assert_eq!(
            parse_palette_intent("ordinary question"),
            PaletteIntent::Ask("ordinary question".into())
        );
        assert_eq!(guide_control_from_action("do-it"), Some(GuideControl::DoIt));
        assert_eq!(guide_control_from_action("unknown"), None);
        let preamble = "The user prefers: bullets.";
        let applied = apply_preferences("Summarize this.", Some(preamble));
        assert!(applied.starts_with(preamble));
        assert_eq!(apply_preferences(&applied, Some(preamble)), applied);
        assert_eq!(apply_preferences("Ask", None), "Ask");
    }

    #[test]
    fn default_config_documents_the_palette_trigger() {
        assert!(default_config_toml().contains(
            "# Unified command palette (daemon mode).\npalette_trigger = \"ctrl+shift+space\""
        ));
    }

    #[test]
    fn mission_graph_requires_complete_acyclic_safe_tasks() {
        let graph = MissionTaskGraph {
            tasks: vec![
                MissionTask {
                    id: "core".into(),
                    title: "Build the core".into(),
                    acceptance: vec!["cargo test -p mice-core".into()],
                    dependencies: vec![],
                    predicted_paths: vec!["crates/mice-core/src/lib.rs".into()],
                },
                MissionTask {
                    id: "cli".into(),
                    title: "Wire the CLI".into(),
                    acceptance: vec!["cargo test -p mice-cli".into()],
                    dependencies: vec!["core".into()],
                    predicted_paths: vec!["crates/mice-cli/src/main.rs".into()],
                },
            ],
        };
        assert!(graph.validate().is_ok());

        let mut parallel_overlap = graph.clone();
        parallel_overlap.tasks[1].dependencies.clear();
        parallel_overlap.tasks[1].predicted_paths = vec!["crates/mice-core/src/lib.rs".into()];
        assert!(
            parallel_overlap
                .validate()
                .unwrap_err()
                .contains("overlapping predicted paths")
        );

        let mut serialized_overlap = graph.clone();
        serialized_overlap.tasks[1].predicted_paths = vec!["crates/mice-core/src/lib.rs".into()];
        assert!(serialized_overlap.validate().is_ok());

        let mut cycle = graph.clone();
        cycle.tasks[0].dependencies.push("cli".into());
        assert!(cycle.validate().unwrap_err().contains("cycle"));

        let mut unsafe_path = graph;
        unsafe_path.tasks[0].predicted_paths = vec!["../outside".into()];
        assert!(unsafe_path.validate().unwrap_err().contains("unsafe"));

        for unsafe_path in ["C:/Windows/System32", "~/.ssh/id_rsa", "src\0hidden"] {
            let mut graph = MissionTaskGraph {
                tasks: vec![MissionTask {
                    id: "core".into(),
                    title: "Build the core".into(),
                    acceptance: vec!["cargo test -p mice-core".into()],
                    dependencies: vec![],
                    predicted_paths: vec![unsafe_path.into()],
                }],
            };
            assert!(graph.validate().unwrap_err().contains("unsafe"));
            graph.tasks[0].predicted_paths = vec!["crates/mice-core".into()];
            assert!(graph.validate().is_ok());
        }

        let oversized = MissionTaskGraph {
            tasks: (0..=MAX_MISSION_TASKS)
                .map(|index| MissionTask {
                    id: format!("task-{index}"),
                    title: format!("Task {index}"),
                    acceptance: vec!["test".into()],
                    dependencies: vec![],
                    predicted_paths: vec![format!("src/{index}.rs")],
                })
                .collect(),
        };
        assert!(oversized.validate().unwrap_err().contains("at most"));
    }

    #[test]
    fn mission_agent_names_have_stable_cli_forms() {
        assert_eq!(
            MissionAgentKind::parse("claude-code"),
            Some(MissionAgentKind::Claude)
        );
        assert_eq!(MissionAgentKind::Antigravity.id(), "antigravity");
        assert!(MissionAgentKind::parse("unknown").is_none());
    }

    #[test]
    fn schedule_time_parsing_and_palette_intent() {
        let base = 1_000_000;
        assert_eq!(parse_schedule_time("10m", base).unwrap(), base + 600);
        assert_eq!(parse_schedule_time("in 2h", base).unwrap(), base + 7200);
        assert_eq!(parse_schedule_time("30s", base).unwrap(), base + 30);
        assert_eq!(parse_schedule_time("tomorrow", base).unwrap(), base + 86400);

        assert_eq!(
            parse_palette_intent("schedule check build in 10m"),
            PaletteIntent::Schedule("check build in 10m".into())
        );
        assert_eq!(
            parse_palette_intent("remind me in 30m to review PR"),
            PaletteIntent::Remind("me in 30m to review PR".into())
        );
    }
}
