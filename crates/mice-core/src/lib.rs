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
impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            trigger: default_trigger(),
            chord_window_ms: default_chord_window(),
            hold_threshold_ms: default_hold_threshold(),
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
    "privacy_mode = \"cloud_allowed\"\ncost_policy = \"cheapest\"\ncloud_model = \"gpt-5.6-luna\"\n# Safe default: gemma3:4b. Alternatives: phi4-mini, gpt-oss:20b (heavy opt-in only).\nlocal_model = \"gemma3:4b\"\n\n[gesture]\ntrigger = \"ctrl+shift+space\"\nchord_window_ms = 120\nhold_threshold_ms = 350\n"
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
        Action::Qa => "Answer the question using the selected content as context.",
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
