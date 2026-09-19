//! TypeSafe Jev System One model integration for Mice intent routing.
//!
//! Provides fast (sub-400ms) predictive decision-making for picking the right
//! local sidekick tool from an orchestrator LLM's natural-language instruction,
//! with confidence-gating (auto-run on high, pause-and-prompt on medium/low)
//! and deterministic heuristic fallback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sidekick::SidekickTaskKind;

pub const DEFAULT_TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const DEFAULT_OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
pub const DEFAULT_MODEL: &str = "typesafe/jev-1.13";
pub const TYPESAFE_FLAGSHIP_MODEL: &str = "jev-latest";
pub const DEFAULT_TIMEOUT_MS: u64 = 400;

/// Confidence threshold above which a tool decision is automatically executed.
pub const HIGH_CONFIDENCE_THRESHOLD: f64 = 0.80;

/// Confidence threshold below which a tool decision is considered low confidence.
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.50;

#[derive(Debug, Error)]
pub enum JevError {
    #[error("I/O or network error calling Jev: {0}")]
    Io(String),
    #[error("Jev decision timed out after {0}ms")]
    Timeout(u64),
    #[error("TypeSafe/Jev API error: {0}")]
    Api(String),
    #[error("Failed to parse Jev response JSON: {0}")]
    Parse(String),
    #[error("No API key configured for Jev/TypeSafe routing")]
    MissingApiKey,
}

/// Confidence classification for decision gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
}

impl ConfidenceLevel {
    pub fn from_confidence(confidence: f64) -> Self {
        if confidence >= HIGH_CONFIDENCE_THRESHOLD {
            Self::High
        } else if confidence >= LOW_CONFIDENCE_THRESHOLD {
            Self::Medium
        } else {
            Self::Low
        }
    }

    pub fn should_auto_run(self) -> bool {
        matches!(self, Self::High)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
        }
    }
}

/// Configuration for the Jev intent router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevConfig {
    #[serde(default = "default_jev_enabled")]
    pub enabled: bool,
    #[serde(default = "default_jev_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_jev_model")]
    pub model: String,
    #[serde(default = "default_jev_timeout_ms")]
    pub timeout_ms: u64,
    /// API key can be set here or via environment variables:
    /// TYPESAFE_API_KEY, JEV_API_KEY, or OPENROUTER_API_KEY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

fn default_jev_enabled() -> bool {
    true
}
fn default_jev_endpoint() -> String {
    DEFAULT_TYPESAFE_ENDPOINT.into()
}
fn default_jev_model() -> String {
    DEFAULT_MODEL.into()
}
fn default_jev_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            enabled: default_jev_enabled(),
            endpoint: default_jev_endpoint(),
            model: default_jev_model(),
            timeout_ms: default_jev_timeout_ms(),
            api_key: None,
        }
    }
}

/// Context passed into Jev state alongside the natural-language command.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IntentRoutingContext {
    pub cwd: PathBuf,
    #[serde(default)]
    pub terminal_history: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmost_app: Option<String>,
}

impl IntentRoutingContext {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            terminal_history: Vec::new(),
            frontmost_app: None,
        }
    }

    pub fn current() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let terminal_history = read_recent_terminal_history(5);
        Self {
            cwd,
            terminal_history,
            frontmost_app: None,
        }
    }

    pub fn with_terminal_history(mut self, history: Vec<String>) -> Self {
        self.terminal_history = history;
        self
    }

    pub fn with_frontmost_app(mut self, app: impl Into<String>) -> Self {
        self.frontmost_app = Some(app.into());
        self
    }
}

/// Structured decision returned by Jev intent routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevRoutingDecision {
    /// The chosen local sidekick tool
    pub tool: SidekickTaskKind,
    /// Probability/confidence score from the choice decision (0.0 to 1.0)
    pub confidence: f64,
    /// High (auto-run), Medium (clarify), or Low (clarify)
    pub confidence_level: ConfidenceLevel,
    /// Full probability distribution over tool candidates
    pub probabilities: HashMap<String, f64>,
    /// Model identifier that produced the decision
    pub model: String,
    /// Rational or criterion summary
    pub reasoning: String,
    /// Whether this was resolved via local deterministic heuristic fallback
    pub is_fallback: bool,
}

/// Builds the JSON request payload for TypeSafe / OpenRouter System One decision endpoint.
pub fn build_decision_request(
    command: &str,
    context: &IntentRoutingContext,
    model: &str,
) -> serde_json::Value {
    let mut state = serde_json::Map::new();
    state.insert(
        "command".to_string(),
        serde_json::Value::String(command.to_string()),
    );
    state.insert(
        "cwd".to_string(),
        serde_json::Value::String(context.cwd.display().to_string()),
    );

    if !context.terminal_history.is_empty() {
        state.insert(
            "terminal_history".to_string(),
            serde_json::Value::String(context.terminal_history.join("\n")),
        );
    }
    if let Some(app) = &context.frontmost_app {
        state.insert(
            "frontmost_app".to_string(),
            serde_json::Value::String(app.clone()),
        );
    }

    serde_json::json!({
        "model": model,
        "state": state,
        "questions": {
            "tool": {
                "type": "choice",
                "instructions": "Pick the single most appropriate local sidekick tool to resolve this instruction.",
                "criteria": {
                    "apply_patch": "Surgical code patching, editing, fixing bugs, lint fixes, applying unified diffs to files",
                    "run_tests": "Local test execution, running test suites, cargo test, verifying test failures or passes",
                    "ast_lookup": "AST symbol search, function definitions, struct definitions, identifier usages, code grep",
                    "semantic_find": "Semantic search across documents, files, invoices, receipts, identity, or personal records",
                    "file_open": "Locating and opening target document or file in system default viewer or desktop application",
                    "file_read": "Targeted file inspection, reading slices or header lines of a file",
                    "knowledge_query": "Traversing the semantic knowledge graph for connected entities, organizations, and relationships",
                    "general": "General routine or multi-step instruction not matching a single specialized tool"
                }
            }
        }
    })
}

/// Parses the JSON response from TypeSafe / OpenRouter decisions endpoint into a JevRoutingDecision.
pub fn parse_decision_response(val: &serde_json::Value) -> Result<JevRoutingDecision, JevError> {
    if let Some(err) = val.get("error") {
        let msg = err
            .as_str()
            .or_else(|| err.get("message").and_then(|m| m.as_str()))
            .unwrap_or("unknown Jev API error");
        return Err(JevError::Api(msg.to_string()));
    }

    let model = val
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();

    let answers = val
        .get("answers")
        .or_else(|| val.get("decisions"))
        .unwrap_or(val);

    let tool_val = answers.get("tool").ok_or_else(|| {
        JevError::Parse("Missing 'tool' decision in response payload".to_string())
    })?;

    let (choice_str, confidence, probabilities) = match tool_val {
        serde_json::Value::String(s) => (s.clone(), 0.85, HashMap::new()),
        serde_json::Value::Object(map) => {
            let choice = map
                .get("choice")
                .or_else(|| map.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("general")
                .to_string();

            let conf = map
                .get("confidence")
                .or_else(|| map.get("score"))
                .or_else(|| map.get("probability"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.75);

            let mut probs = HashMap::new();
            if let Some(serde_json::Value::Object(p_map)) = map.get("probabilities") {
                for (k, v) in p_map {
                    if let Some(score) = v.as_f64() {
                        probs.insert(k.clone(), score);
                    }
                }
            }

            (choice, conf, probs)
        }
        _ => ("general".to_string(), 0.50, HashMap::new()),
    };

    let tool = SidekickTaskKind::from_tool_name(&choice_str);
    let confidence_level = ConfidenceLevel::from_confidence(confidence);
    let reasoning = format!(
        "Jev selected '{}' with {:.1}% confidence ({})",
        tool.tool_name(),
        confidence * 100.0,
        confidence_level.label()
    );

    Ok(JevRoutingDecision {
        tool,
        confidence,
        confidence_level,
        probabilities,
        model,
        reasoning,
        is_fallback: false,
    })
}

/// Deterministic heuristic fallback intent routing when Jev is disabled, unavailable, or times out.
pub fn heuristic_fallback_route(
    command: &str,
    _context: &IntentRoutingContext,
) -> JevRoutingDecision {
    let lower = command.trim().to_lowercase();

    // High-priority pattern checks
    let (tool, confidence, reasoning) = if lower.contains("test")
        || lower.contains("cargo test")
        || lower.contains("failing")
        || lower.contains("failure")
        || lower.contains("assert")
    {
        (
            SidekickTaskKind::TestRun,
            0.88,
            "Deterministic fallback: matched testing keywords".into(),
        )
    } else if lower.contains("fix")
        || lower.contains("patch")
        || lower.contains("lint")
        || lower.contains("bug")
        || lower.contains("replace")
        || lower.contains("modify")
        || lower.contains("diff")
    {
        (
            SidekickTaskKind::CodePatch,
            0.88,
            "Deterministic fallback: matched code patching/fix keywords".into(),
        )
    } else if lower.starts_with("open ")
        || lower.contains("open file")
        || lower.contains("launch")
        || lower.contains("view in")
    {
        (
            SidekickTaskKind::FileOpen,
            0.85,
            "Deterministic fallback: matched file open keywords".into(),
        )
    } else if lower.contains("ast")
        || lower.contains("symbol")
        || lower.contains("definition")
        || lower.contains("where is")
        || lower.contains("reference")
        || lower.contains("function ")
        || lower.contains("struct ")
        || lower.contains("enum ")
    {
        (
            SidekickTaskKind::AstSearch,
            0.85,
            "Deterministic fallback: matched AST/symbol search keywords".into(),
        )
    } else if lower.contains("aadhaar")
        || lower.contains("invoice")
        || lower.contains("receipt")
        || lower.contains("bill")
        || lower.contains("salary")
        || lower.contains("tax")
        || lower.contains("passport")
        || lower.contains("statement")
        || lower.contains("resume")
    {
        (
            SidekickTaskKind::SemanticSearch,
            0.85,
            "Deterministic fallback: matched semantic document keywords".into(),
        )
    } else if lower.contains("knowledge")
        || lower.contains("graph")
        || lower.contains("entity")
        || lower.contains("entities")
        || lower.contains("relationship")
    {
        (
            SidekickTaskKind::KnowledgeQuery,
            0.85,
            "Deterministic fallback: matched knowledge graph keywords".into(),
        )
    } else if lower.starts_with("read ")
        || lower.contains("file read")
        || lower.contains("slice")
        || lower.contains("cat ")
    {
        (
            SidekickTaskKind::FileRead,
            0.82,
            "Deterministic fallback: matched file read keywords".into(),
        )
    } else {
        (
            SidekickTaskKind::General,
            0.45,
            "Deterministic fallback: ambiguous or general command".into(),
        )
    };

    let confidence_level = ConfidenceLevel::from_confidence(confidence);

    JevRoutingDecision {
        tool,
        confidence,
        confidence_level,
        probabilities: HashMap::new(),
        model: "deterministic_heuristic_fallback".into(),
        reasoning,
        is_fallback: true,
    }
}

/// Injectable client trait for the Jev System One model.
pub trait JevClient: Send + Sync {
    fn decide(
        &self,
        command: &str,
        context: &IntentRoutingContext,
    ) -> Result<JevRoutingDecision, JevError>;
}

/// Real HTTP client that calls TypeSafe / OpenRouter System One endpoint.
#[derive(Debug, Clone)]
pub struct HttpJevClient {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub timeout_ms: u64,
}

impl HttpJevClient {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            model: model.into(),
            timeout_ms,
        }
    }
}

impl JevClient for HttpJevClient {
    fn decide(
        &self,
        command: &str,
        context: &IntentRoutingContext,
    ) -> Result<JevRoutingDecision, JevError> {
        let payload = build_decision_request(command, context, &self.model);

        let response = ureq::post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .timeout(Duration::from_millis(self.timeout_ms))
            .send_json(payload);

        match response {
            Ok(resp) => {
                let body: serde_json::Value = resp
                    .into_json()
                    .map_err(|e| JevError::Parse(e.to_string()))?;
                parse_decision_response(&body)
            }
            Err(ureq::Error::Status(status, resp)) => {
                let err_text = resp.into_string().unwrap_or_default();
                Err(JevError::Api(format!("HTTP {status}: {err_text}")))
            }
            Err(ureq::Error::Transport(transport)) => {
                let desc = transport.to_string();
                if desc.to_lowercase().contains("timeout")
                    || desc.to_lowercase().contains("timed out")
                {
                    Err(JevError::Timeout(self.timeout_ms))
                } else {
                    Err(JevError::Io(desc))
                }
            }
        }
    }
}

/// Type alias for mock decision closures.
pub type MockDecisionFn =
    Arc<dyn Fn(&str, &IntentRoutingContext) -> Result<JevRoutingDecision, JevError> + Send + Sync>;

/// Mock client for deterministic, network-free unit tests.
#[derive(Clone)]
pub struct MockJevClient {
    mock_fn: MockDecisionFn,
}

impl MockJevClient {
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&str, &IntentRoutingContext) -> Result<JevRoutingDecision, JevError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            mock_fn: Arc::new(f),
        }
    }

    pub fn with_fixed_decision(tool: SidekickTaskKind, confidence: f64) -> Self {
        Self::new(move |_cmd, _ctx| {
            Ok(JevRoutingDecision {
                tool,
                confidence,
                confidence_level: ConfidenceLevel::from_confidence(confidence),
                probabilities: HashMap::new(),
                model: "mock/jev-1.13".into(),
                reasoning: "Mock decision".into(),
                is_fallback: false,
            })
        })
    }

    pub fn with_error(error: JevError) -> Self {
        let err_str = error.to_string();
        Self::new(move |_cmd, _ctx| Err(JevError::Api(err_str.clone())))
    }
}

impl JevClient for MockJevClient {
    fn decide(
        &self,
        command: &str,
        context: &IntentRoutingContext,
    ) -> Result<JevRoutingDecision, JevError> {
        (self.mock_fn)(command, context)
    }
}

/// Router orchestrating Jev decisions with graceful heuristic fallback.
pub struct JevIntentRouter {
    client: Option<Arc<dyn JevClient>>,
    fallback_enabled: bool,
}

impl JevIntentRouter {
    pub fn new(client: Option<Arc<dyn JevClient>>) -> Self {
        Self {
            client,
            fallback_enabled: true,
        }
    }

    pub fn without_fallback(client: Option<Arc<dyn JevClient>>) -> Self {
        Self {
            client,
            fallback_enabled: false,
        }
    }

    /// Primary routing method. Attempts Jev client call, gracefully falling back on error or absence.
    pub fn route(&self, command: &str, context: &IntentRoutingContext) -> JevRoutingDecision {
        if let Some(client) = &self.client {
            match client.decide(command, context) {
                Ok(decision) => return decision,
                Err(_err) => {
                    // Log or handle error, fall through to deterministic fallback if enabled
                    if !self.fallback_enabled {
                        return JevRoutingDecision {
                            tool: SidekickTaskKind::General,
                            confidence: 0.0,
                            confidence_level: ConfidenceLevel::Low,
                            probabilities: HashMap::new(),
                            model: "error_no_fallback".into(),
                            reasoning: format!("Jev routing failed: {_err}"),
                            is_fallback: true,
                        };
                    }
                }
            }
        }

        heuristic_fallback_route(command, context)
    }
}

/// Read recent terminal history lines safely without errors.
pub fn read_recent_terminal_history(max_lines: usize) -> Vec<String> {
    let mut history_paths = Vec::new();
    if let Ok(histfile) = std::env::var("HISTFILE") {
        history_paths.push(PathBuf::from(histfile));
    }
    if let Ok(home) = std::env::var("HOME") {
        let home_path = Path::new(&home);
        history_paths.push(home_path.join(".zsh_history"));
        history_paths.push(home_path.join(".bash_history"));
    }

    for path in history_paths {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let lines: Vec<String> = content
                .lines()
                .rev()
                .filter_map(|l| {
                    let trimmed = l.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    // Strip zsh extended history timestamp prefix: ": 1600000000:0;cmd"
                    if let Some(cmd) = trimmed.strip_prefix(": ")
                        && let Some(idx) = cmd.find(';')
                    {
                        return Some(cmd[idx + 1..].trim().to_string());
                    }
                    Some(trimmed.to_string())
                })
                .take(max_lines)
                .collect();

            if !lines.is_empty() {
                let mut reversed = lines;
                reversed.reverse();
                return reversed;
            }
        }
    }

    Vec::new()
}

/// Resolves Jev credentials from environment variables or configuration.
/// Priority order:
/// 1. `TYPESAFE_API_KEY` (targets TypeSafe endpoint)
/// 2. `JEV_API_KEY` (targets TypeSafe endpoint)
/// 3. `OPENROUTER_API_KEY` (targets OpenRouter endpoint)
/// 4. `config.jev.api_key`
pub fn resolve_jev_credentials(config: Option<&JevConfig>) -> Option<(String, String, String)> {
    if let Some(cfg) = config
        && !cfg.enabled
    {
        return None;
    }

    let mut api_key = None;
    let mut endpoint = None;
    let mut model = None;

    if let Ok(key) = std::env::var("TYPESAFE_API_KEY")
        && !key.trim().is_empty()
    {
        api_key = Some(key.trim().to_string());
        endpoint = Some(DEFAULT_TYPESAFE_ENDPOINT.to_string());
        model = Some(DEFAULT_MODEL.to_string());
    } else if let Ok(key) = std::env::var("JEV_API_KEY")
        && !key.trim().is_empty()
    {
        api_key = Some(key.trim().to_string());
        endpoint = Some(DEFAULT_TYPESAFE_ENDPOINT.to_string());
        model = Some(DEFAULT_MODEL.to_string());
    } else if let Ok(key) = std::env::var("OPENROUTER_API_KEY")
        && !key.trim().is_empty()
    {
        api_key = Some(key.trim().to_string());
        endpoint = Some(DEFAULT_OPENROUTER_ENDPOINT.to_string());
        model = Some(DEFAULT_MODEL.to_string());
    } else if let Some(cfg) = config
        && let Some(key) = &cfg.api_key
        && !key.trim().is_empty()
    {
        api_key = Some(key.trim().to_string());
        endpoint = Some(cfg.endpoint.clone());
        model = Some(cfg.model.clone());
    }

    // Allow explicit endpoint or model override via environment
    if let Ok(ep) = std::env::var("TYPESAFE_ENDPOINT").or_else(|_| std::env::var("JEV_ENDPOINT"))
        && !ep.trim().is_empty()
    {
        endpoint = Some(ep.trim().to_string());
    }
    if let Ok(mdl) = std::env::var("JEV_MODEL")
        && !mdl.trim().is_empty()
    {
        model = Some(mdl.trim().to_string());
    }

    let key = api_key?;
    let ep = endpoint.unwrap_or_else(|| DEFAULT_TYPESAFE_ENDPOINT.to_string());
    let mdl = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());

    Some((key, ep, mdl))
}

/// Create a configured JevIntentRouter ready for production use.
pub fn create_default_router(config: Option<&JevConfig>) -> JevIntentRouter {
    if let Some(cfg) = config
        && !cfg.enabled
    {
        return JevIntentRouter::new(None);
    }
    let timeout_ms = config.map(|c| c.timeout_ms).unwrap_or(DEFAULT_TIMEOUT_MS);
    let client = resolve_jev_credentials(config).map(|(key, endpoint, model)| {
        let c: Arc<dyn JevClient> = Arc::new(HttpJevClient::new(endpoint, key, model, timeout_ms));
        c
    });
    JevIntentRouter::new(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_confidence_level_classification() {
        assert_eq!(
            ConfidenceLevel::from_confidence(0.95),
            ConfidenceLevel::High
        );
        assert_eq!(
            ConfidenceLevel::from_confidence(0.80),
            ConfidenceLevel::High
        );
        assert!(ConfidenceLevel::from_confidence(0.80).should_auto_run());

        assert_eq!(
            ConfidenceLevel::from_confidence(0.79),
            ConfidenceLevel::Medium
        );
        assert_eq!(
            ConfidenceLevel::from_confidence(0.50),
            ConfidenceLevel::Medium
        );
        assert!(!ConfidenceLevel::from_confidence(0.79).should_auto_run());

        assert_eq!(ConfidenceLevel::from_confidence(0.49), ConfidenceLevel::Low);
        assert_eq!(ConfidenceLevel::from_confidence(0.10), ConfidenceLevel::Low);
        assert!(!ConfidenceLevel::from_confidence(0.49).should_auto_run());
    }

    #[test]
    fn test_build_decision_request_structure() {
        let ctx = IntentRoutingContext {
            cwd: PathBuf::from("/Users/test/project"),
            terminal_history: vec!["cargo check".into(), "git status".into()],
            frontmost_app: Some("iTerm2".into()),
        };

        let req = build_decision_request("fix the lint error in lib.rs", &ctx, DEFAULT_MODEL);

        assert_eq!(req["model"], DEFAULT_MODEL);
        assert_eq!(req["state"]["command"], "fix the lint error in lib.rs");
        assert_eq!(req["state"]["cwd"], "/Users/test/project");
        assert_eq!(req["state"]["terminal_history"], "cargo check\ngit status");
        assert_eq!(req["state"]["frontmost_app"], "iTerm2");
        assert_eq!(req["questions"]["tool"]["type"], "choice");
        assert!(req["questions"]["tool"]["criteria"]["apply_patch"].is_string());
        assert!(req["questions"]["tool"]["criteria"]["run_tests"].is_string());
        assert!(req["questions"]["tool"]["criteria"]["ast_lookup"].is_string());
    }

    #[test]
    fn test_parse_decision_response_standard_typesafe() {
        let json_str = r#"{
            "model": "jev-latest",
            "answers": {
                "tool": {
                    "type": "choice",
                    "choice": "apply_patch",
                    "confidence": 0.94,
                    "probabilities": {
                        "apply_patch": 0.94,
                        "ast_lookup": 0.04,
                        "run_tests": 0.02
                    }
                }
            },
            "usage": { "input_tokens": 120, "output_tokens": 1 }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dec = parse_decision_response(&val).unwrap();

        assert_eq!(dec.tool, SidekickTaskKind::CodePatch);
        assert!((dec.confidence - 0.94).abs() < 1e-6);
        assert_eq!(dec.confidence_level, ConfidenceLevel::High);
        assert!(dec.confidence_level.should_auto_run());
        assert_eq!(dec.probabilities.get("apply_patch"), Some(&0.94));
        assert!(!dec.is_fallback);
    }

    #[test]
    fn test_parse_decision_response_openrouter_format() {
        let json_str = r#"{
            "model": "typesafe/jev-1.13",
            "decisions": {
                "tool": {
                    "choice": "ast_lookup",
                    "score": 0.72
                }
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dec = parse_decision_response(&val).unwrap();

        assert_eq!(dec.tool, SidekickTaskKind::AstSearch);
        assert!((dec.confidence - 0.72).abs() < 1e-6);
        assert_eq!(dec.confidence_level, ConfidenceLevel::Medium);
        assert!(!dec.confidence_level.should_auto_run());
    }

    #[test]
    fn test_parse_decision_response_error() {
        let json_str = r#"{
            "error": {
                "message": "Model typesafe/jev-1.13 is overloaded"
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let err = parse_decision_response(&val).unwrap_err();
        assert!(err.to_string().contains("overloaded"));
    }

    #[test]
    fn test_parse_decision_response_string_error() {
        let json_str = r#"{
            "error": "Unauthorized"
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let err = parse_decision_response(&val).unwrap_err();
        assert!(err.to_string().contains("Unauthorized"));
    }

    #[test]
    fn test_heuristic_fallback_routing() {
        let ctx = IntentRoutingContext::new(PathBuf::from("."));

        // Code patch test
        let dec_patch = heuristic_fallback_route("fix the lint error in lib.rs", &ctx);
        assert_eq!(dec_patch.tool, SidekickTaskKind::CodePatch);
        assert!(dec_patch.confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert!(dec_patch.is_fallback);

        // Test run
        let dec_test = heuristic_fallback_route("run cargo test in workspace", &ctx);
        assert_eq!(dec_test.tool, SidekickTaskKind::TestRun);
        assert!(dec_test.confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert!(dec_test.is_fallback);

        // AST search
        let dec_ast = heuristic_fallback_route("find symbol SidekickEngine in crates", &ctx);
        assert_eq!(dec_ast.tool, SidekickTaskKind::AstSearch);
        assert!(dec_ast.is_fallback);

        // Semantic search
        let dec_sem = heuristic_fallback_route("locate my electricity bill invoice", &ctx);
        assert_eq!(dec_sem.tool, SidekickTaskKind::SemanticSearch);
        assert!(dec_sem.is_fallback);

        // File open
        let dec_open = heuristic_fallback_route("open 12th report card in system viewer", &ctx);
        assert_eq!(dec_open.tool, SidekickTaskKind::FileOpen);
        assert!(dec_open.is_fallback);

        // Ambiguous / general
        let dec_gen = heuristic_fallback_route("help me think about stuff", &ctx);
        assert_eq!(dec_gen.tool, SidekickTaskKind::General);
        assert_eq!(dec_gen.confidence_level, ConfidenceLevel::Low);
        assert!(!dec_gen.confidence_level.should_auto_run());
    }

    #[test]
    fn test_mock_jev_client_routing() {
        let mock_client = Arc::new(MockJevClient::with_fixed_decision(
            SidekickTaskKind::CodePatch,
            0.92,
        ));
        let router = JevIntentRouter::new(Some(mock_client));
        let ctx = IntentRoutingContext::new(PathBuf::from("."));

        let decision = router.route("fix the lint error in lib.rs", &ctx);
        assert_eq!(decision.tool, SidekickTaskKind::CodePatch);
        assert_eq!(decision.confidence_level, ConfidenceLevel::High);
        assert!(decision.confidence_level.should_auto_run());
        assert!(!decision.is_fallback);
    }

    #[test]
    fn test_fallback_when_jev_client_fails() {
        let mock_client = Arc::new(MockJevClient::with_error(JevError::Timeout(400)));
        let router = JevIntentRouter::new(Some(mock_client));
        let ctx = IntentRoutingContext::new(PathBuf::from("."));

        // When the client fails, the router should gracefully fall back to heuristic routing!
        let decision = router.route("fix the lint error in lib.rs", &ctx);
        assert_eq!(decision.tool, SidekickTaskKind::CodePatch);
        assert!(decision.is_fallback);
    }

    #[test]
    fn test_fallback_when_no_client_or_key() {
        let router = JevIntentRouter::new(None);
        let ctx = IntentRoutingContext::new(PathBuf::from("."));

        let decision = router.route("run tests for mice-core", &ctx);
        assert_eq!(decision.tool, SidekickTaskKind::TestRun);
        assert!(decision.is_fallback);
    }

    #[test]
    fn test_disabled_jev_config_bypasses_credentials() {
        let disabled_cfg = JevConfig {
            enabled: false,
            endpoint: DEFAULT_TYPESAFE_ENDPOINT.to_string(),
            model: DEFAULT_MODEL.to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            api_key: Some("explicit_key".to_string()),
        };

        assert!(resolve_jev_credentials(Some(&disabled_cfg)).is_none());
        let router = create_default_router(Some(&disabled_cfg));
        let ctx = IntentRoutingContext::new(PathBuf::from("."));
        let decision = router.route("fix the lint error in lib.rs", &ctx);
        assert_eq!(decision.tool, SidekickTaskKind::CodePatch);
        assert!(decision.is_fallback);
    }
}
