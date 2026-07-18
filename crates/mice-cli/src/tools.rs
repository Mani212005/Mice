//! Deterministic-first command registry for MICE's execution-manager layer.
//!
//! This module deliberately keeps a runner trait at the boundary so unit tests
//! never need Node, GitHub, Chrome, or a network connection.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use mice_core::estimate_tokens;
use serde_json::{Value, json};

pub const DEFAULT_RETURN_TOKENS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnly,
    Mutating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistillPolicy {
    Never,
    IfLarge,
    Always,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub kind: ToolKind,
    pub distill: DistillPolicy,
    pub program: &'static str,
    pub availability_program: &'static str,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub session_name: String,
    pub output_budget_tokens: usize,
    pub careful_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub raw: String,
    pub truncated: bool,
    pub full_output_ref: Option<String>,
    pub needs_distillation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ToolError {}

pub trait CommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<String, ToolError>;
    fn available(&self, program: &str) -> bool;
}

pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        environment: &[(String, String)],
    ) -> Result<String, ToolError> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            // Provider keys must never be inherited by third-party CLIs.
            .env_clear()
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .output()
            .map_err(|error| ToolError(format!("Could not run {program}: {error}")))?;
        if !output.status.success() {
            return Err(ToolError(format!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn available(&self, program: &str) -> bool {
        Command::new(program).arg("--version").output().is_ok()
    }
}

pub fn specs() -> &'static [ToolSpec] {
    &[
        ToolSpec {
            name: "git.status",
            description: "Read the current repository status.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::Never,
            program: "git",
            availability_program: "git",
        },
        ToolSpec {
            name: "git.log",
            description: "Read recent commits in the current repository.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "git",
            availability_program: "git",
        },
        ToolSpec {
            name: "git.diff",
            description: "Read the current uncommitted diff.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "git",
            availability_program: "git",
        },
        ToolSpec {
            name: "git.branch",
            description: "Read current branches.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::Never,
            program: "git",
            availability_program: "git",
        },
        ToolSpec {
            name: "repo.grep",
            description: "Search repository text with a fixed pattern.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "rg",
            availability_program: "rg",
        },
        ToolSpec {
            name: "github.pr_list",
            description: "List pull requests through gh-axi or gh.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "gh-axi",
            availability_program: "npx",
        },
        ToolSpec {
            name: "github.pr_view",
            description: "Read a pull request through gh-axi or gh.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "gh-axi",
            availability_program: "npx",
        },
        ToolSpec {
            name: "github.pr_checks",
            description: "Read pull-request CI checks through gh-axi or gh.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "gh-axi",
            availability_program: "npx",
        },
        ToolSpec {
            name: "github.issue_list",
            description: "List GitHub issues through gh-axi or gh.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "gh-axi",
            availability_program: "npx",
        },
        ToolSpec {
            name: "browser.open",
            description: "Open a URL in the isolated Chrome AXI session.",
            kind: ToolKind::Mutating,
            distill: DistillPolicy::Never,
            program: "npx",
            availability_program: "npx",
        },
        ToolSpec {
            name: "browser.snapshot",
            description: "Capture the current Chrome AXI accessibility snapshot.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::IfLarge,
            program: "npx",
            availability_program: "npx",
        },
        ToolSpec {
            name: "browser.click",
            description: "Click a verified Chrome AXI uid.",
            kind: ToolKind::Mutating,
            distill: DistillPolicy::Never,
            program: "npx",
            availability_program: "npx",
        },
        ToolSpec {
            name: "browser.fill",
            description: "Fill a verified Chrome AXI uid.",
            kind: ToolKind::Mutating,
            distill: DistillPolicy::Never,
            program: "npx",
            availability_program: "npx",
        },
        ToolSpec {
            name: "browser.press",
            description: "Press a key in the Chrome AXI session.",
            kind: ToolKind::Mutating,
            distill: DistillPolicy::Never,
            program: "npx",
            availability_program: "npx",
        },
        ToolSpec {
            name: "browser.scroll",
            description: "Scroll the Chrome AXI session.",
            kind: ToolKind::Mutating,
            distill: DistillPolicy::Never,
            program: "npx",
            availability_program: "npx",
        },
        ToolSpec {
            name: "quota.status",
            description: "Read locally available editor quota windows.",
            kind: ToolKind::ReadOnly,
            distill: DistillPolicy::Never,
            program: "npx",
            availability_program: "npx",
        },
    ]
}

pub fn tool_schema() -> Vec<Value> {
    specs()
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": {"type": "object", "additionalProperties": true}
            })
        })
        .collect()
}

pub fn availability(runner: &impl CommandRunner) -> Vec<(String, bool)> {
    specs()
        .iter()
        .map(|spec| {
            // Keep the declared executable part of the registry contract even
            // when a specific adapter selects a compatible fallback at run time.
            let _declared_program = spec.program;
            (
                spec.name.into(),
                runner.available(spec.availability_program),
            )
        })
        .collect()
}

pub fn run(
    runner: &impl CommandRunner,
    call: &ToolCall,
    context: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let spec = specs()
        .iter()
        .find(|spec| spec.name == call.name)
        .ok_or_else(|| ToolError(format!("Unknown tool `{}`", call.name)))?;
    if spec.kind == ToolKind::Mutating && context.careful_mode {
        return Err(ToolError(format!(
            "`{}` needs confirmation because careful mode is enabled.",
            call.name
        )));
    }
    if is_sensitive_browser_call(call) {
        return Err(ToolError(
            "MICE will not fill credentials, one-time codes, or payment data through a browser tool. Please do this step yourself."
                .into(),
        ));
    }
    let (mut program, mut args) = command_for(spec, &call.args)?;
    if program == "gh-axi" && !runner.available("gh-axi") {
        if runner.available("npx") {
            args.splice(0..0, ["-y".into(), "gh-axi".into()]);
            program = "npx".into();
        } else {
            program = "gh".into();
        }
    }
    let raw = runner.run(
        &program,
        &args,
        &context.working_dir,
        &sanitized_env(&context.session_name),
    )?;
    let (text, truncated) = bound_output(&raw, context.output_budget_tokens);
    Ok(ToolOutput {
        text,
        raw,
        truncated,
        full_output_ref: None,
        needs_distillation: spec.distill == DistillPolicy::Always
            || (spec.distill == DistillPolicy::IfLarge && truncated),
    })
}

fn is_sensitive_browser_call(call: &ToolCall) -> bool {
    if call.name != "browser.fill" {
        return false;
    }
    ["uid", "text"]
        .into_iter()
        .filter_map(|key| call.args.get(key).and_then(Value::as_str))
        .any(|value| {
            let value = value.to_ascii_lowercase();
            [
                "password",
                "passcode",
                "otp",
                "one-time",
                "credit card",
                "card number",
                "cvv",
                "ssn",
            ]
            .iter()
            .any(|needle| value.contains(needle))
        })
}

fn command_for(spec: &ToolSpec, args: &Value) -> Result<(String, Vec<String>), ToolError> {
    let string = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError(format!("`{}` requires string argument `{key}`", spec.name)))
    };
    let optional = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    let direct = |subcommand: &[&str]| {
        Ok((
            "git".into(),
            subcommand.iter().map(|value| (*value).into()).collect(),
        ))
    };
    match spec.name {
        "git.status" => direct(&["status", "--short", "--branch"]),
        "git.log" => direct(&["log", "--oneline", "-20"]),
        "git.diff" => direct(&["diff", "--no-ext-diff"]),
        "git.branch" => direct(&["branch", "--verbose", "--no-abbrev"]),
        "repo.grep" => {
            let mut command = vec![
                "--line-number".into(),
                "--no-heading".into(),
                string("pattern")?.into(),
            ];
            if let Some(path) = optional("path") {
                command.push(path);
            }
            Ok(("rg".into(), command))
        }
        name if name.starts_with("github.") => github_command(name, args),
        name if name.starts_with("browser.") => {
            let action = name.trim_start_matches("browser.");
            let mut command = vec!["-y".into(), "chrome-devtools-axi".into(), action.into()];
            match action {
                "open" => command.push(string("url")?.into()),
                "click" => command.push(string("uid")?.into()),
                "fill" => {
                    command.push(string("uid")?.into());
                    command.push(string("text")?.into());
                }
                "press" => command.push(string("key")?.into()),
                "scroll" => command.push(optional("direction").unwrap_or_else(|| "down".into())),
                "snapshot" => {}
                _ => return Err(ToolError(format!("Unsupported browser action `{action}`"))),
            }
            Ok(("npx".into(), command))
        }
        "quota.status" => Ok((
            "npx".into(),
            vec!["-y".into(), "quota-axi".into(), "--json".into()],
        )),
        _ => Err(ToolError(format!("Unsupported tool `{}`", spec.name))),
    }
}

fn github_command(name: &str, args: &Value) -> Result<(String, Vec<String>), ToolError> {
    let action = match name {
        "github.pr_list" => "pr list",
        "github.pr_view" => "pr view",
        "github.pr_checks" => "pr checks",
        "github.issue_list" => "issue list",
        _ => return Err(ToolError(format!("Unsupported GitHub tool `{name}`"))),
    };
    // gh-axi is preferred. Its argv mirrors gh for this read-only subset; if
    // unavailable SystemRunner reports it and the caller can select gh fallback.
    let mut command = action.split(' ').map(str::to_owned).collect::<Vec<_>>();
    if let Some(number) = args.get("number").and_then(Value::as_i64) {
        command.push(number.to_string());
    }
    if let Some(repo) = args.get("repo").and_then(Value::as_str) {
        command.extend(["--repo".into(), repo.into()]);
    }
    Ok(("gh-axi".into(), command))
}

pub fn sanitized_env(session_name: &str) -> Vec<(String, String)> {
    let mut allowed = Vec::new();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = env::var_os(key) {
            allowed.push((key.into(), value.to_string_lossy().into_owned()));
        }
    }
    for (key, value) in env::vars() {
        if key.starts_with("CHROME_DEVTOOLS_AXI_") {
            allowed.push((key, value));
        }
    }
    allowed.push(("CHROME_DEVTOOLS_AXI_SESSION".into(), session_name.into()));
    allowed
}

pub fn bound_output(raw: &str, budget_tokens: usize) -> (String, bool) {
    if estimate_tokens(raw) <= budget_tokens {
        return (raw.into(), false);
    }
    let character_budget = budget_tokens.saturating_mul(4).max(64);
    let head = character_budget * 2 / 3;
    let tail = character_budget - head;
    let first = raw.chars().take(head).collect::<String>();
    let last = raw
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    (
        format!("{first}\n\n… [output truncated; full output is cached] …\n\n{last}"),
        true,
    )
}

pub fn stable_tool_prompt() -> String {
    let mut descriptions = specs()
        .iter()
        .map(|spec| format!("{}: {}", spec.name, spec.description))
        .collect::<Vec<_>>();
    descriptions.sort();
    descriptions.join("\n")
}

pub fn call_fingerprint(call: &ToolCall, state: &str) -> String {
    // A stable, non-secret cache key; JSON object keys are normalized first.
    let canonical = canonical_json(&call.args);
    format!("{}:{}:{state}", call.name, canonical)
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!("{key}:{}", canonical_json(&map[key])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockRunner {
        output: String,
    }
    impl CommandRunner for MockRunner {
        fn run(
            &self,
            _: &str,
            _: &[String],
            _: &Path,
            _: &[(String, String)],
        ) -> Result<String, ToolError> {
            Ok(self.output.clone())
        }
        fn available(&self, _: &str) -> bool {
            true
        }
    }

    #[test]
    fn output_contract_keeps_head_and_tail() {
        let source = format!("start{}finish", " middle".repeat(500));
        let (value, truncated) = bound_output(&source, 20);
        assert!(truncated);
        assert!(value.starts_with("start"));
        assert!(value.ends_with("finish"));
    }

    #[test]
    fn runner_environment_never_contains_provider_keys() {
        let environment = sanitized_env("agent-a");
        assert!(environment.iter().all(|(key, _)| !key.ends_with("API_KEY")));
        assert!(
            environment
                .iter()
                .any(|(key, value)| key == "CHROME_DEVTOOLS_AXI_SESSION" && value == "agent-a")
        );
    }

    #[test]
    fn readonly_tools_run_without_a_model() {
        let output = run(
            &MockRunner {
                output: "ok".into(),
            },
            &ToolCall {
                name: "git.status".into(),
                args: json!({}),
            },
            &ToolContext {
                working_dir: PathBuf::from("."),
                session_name: "s".into(),
                output_budget_tokens: 300,
                careful_mode: true,
            },
        )
        .unwrap();
        assert_eq!(output.text, "ok");
    }

    #[test]
    fn browser_sensitive_fills_are_never_sent_to_a_runner() {
        let result = run(
            &MockRunner {
                output: "should not run".into(),
            },
            &ToolCall {
                name: "browser.fill".into(),
                args: json!({"uid":"password-field", "text":"secret"}),
            },
            &ToolContext {
                working_dir: PathBuf::from("."),
                session_name: "s".into(),
                output_budget_tokens: 300,
                careful_mode: false,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn github_prefers_npx_axi_before_plain_gh() {
        struct NpxOnly;
        impl CommandRunner for NpxOnly {
            fn run(
                &self,
                program: &str,
                args: &[String],
                _: &Path,
                _: &[(String, String)],
            ) -> Result<String, ToolError> {
                assert_eq!(program, "npx");
                assert_eq!(&args[..2], ["-y", "gh-axi"]);
                Ok("[]".into())
            }
            fn available(&self, program: &str) -> bool {
                program == "npx"
            }
        }
        let output = run(
            &NpxOnly,
            &ToolCall {
                name: "github.pr_list".into(),
                args: json!({}),
            },
            &ToolContext {
                working_dir: PathBuf::from("."),
                session_name: "s".into(),
                output_budget_tokens: 300,
                careful_mode: false,
            },
        )
        .unwrap();
        assert_eq!(output.text, "[]");
    }
}
