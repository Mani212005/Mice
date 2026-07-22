//! M16: MICE as an MCP client.
//!
//! Connects over stdio to external MCP servers the user has explicitly
//! granted in `config.toml` (`[[mcp.servers]]` with `enabled = true`).
//! Boundaries enforced here, in code:
//!
//! - A server process receives a scrubbed environment: provider API keys and
//!   everything else beyond PATH/HOME/LANG/TMPDIR never reach it.
//! - Imported tools surface only as *text answers* rendered to the person.
//!   They have no route into MICE's browser bridge, tool registry, clipboard,
//!   or any mutation surface, and links in their output are shown, never
//!   fetched.
//! - Server output is sanitized (control sequences stripped) and bounded
//!   before display, and every request has a hard timeout with the child
//!   killed on drop.

use std::{
    io::{BufReader, Read, Write},
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use mice_core::McpServerConfig;
use serde_json::{Value, json};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MCP_LINE_BYTES: usize = 64 * 1024;
const MAX_RESULT_CHARS: usize = 8_000;
const TRUNCATION_NOTICE: &str = "… (external result truncated)";
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

pub struct McpServerProcess {
    pub name: String,
    child: Child,
    writer: Arc<Mutex<ChildStdin>>,
    lines: mpsc::Receiver<Result<String, String>>,
    next_id: u64,
    timeout: Duration,
    terminated: bool,
}

impl Drop for McpServerProcess {
    fn drop(&mut self) {
        // `terminate` already reaped the tree; running the group kill again
        // here would signal a freed (possibly recycled) process-group id.
        if !self.terminated {
            self.terminate();
        }
    }
}

impl McpServerProcess {
    /// Spawn and handshake a granted server. The child gets a scrubbed
    /// environment so MICE's provider keys can never leak to external code.
    pub fn spawn(config: &McpServerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_with_timeout(config, DEFAULT_REQUEST_TIMEOUT)
    }

    fn spawn_with_timeout(
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .env_clear()
            .envs(
                ["PATH", "HOME", "LANG", "TMPDIR"]
                    .iter()
                    .filter_map(|key| std::env::var_os(key).map(|value| (*key, value))),
            )
            // A dedicated process group makes timeout/drop cleanup reach the
            // whole descendant tree, not just the direct child.
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                format!(
                    "Could not start MCP server `{}`: {}",
                    sanitize_external_text(&config.name),
                    sanitize_external_text(&error.to_string())
                )
            })?;
        let writer = child
            .stdin
            .take()
            .ok_or("MCP server stdin was not available")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("MCP server stdout was not available")?;
        // Bound queued valid messages as well as individual line length. A
        // granted but faulty server cannot accumulate unlimited output while
        // MICE is waiting on another request.
        let (sender, lines) = mpsc::sync_channel(16);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let line = read_bounded_line(&mut reader);
                let done = line.is_err() || line.as_ref().is_ok_and(Option::is_none);
                let message = match line {
                    Ok(Some(line)) => Ok(line),
                    Ok(None) => break,
                    Err(error) => Err(error.to_string()),
                };
                if sender.send(message).is_err() || done {
                    break;
                }
            }
        });
        let mut server = Self {
            name: config.name.clone(),
            child,
            writer: Arc::new(Mutex::new(writer)),
            lines,
            next_id: 0,
            timeout,
            terminated: false,
        };
        server.initialize()?;
        Ok(server)
    }

    fn initialize(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "mice", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        if result.get("protocolVersion").is_none() {
            return Err(format!(
                "MCP server `{}` sent an invalid initialize response",
                sanitize_external_text(&self.name)
            )
            .into());
        }
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    pub fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, Box<dyn std::error::Error>> {
        let result = self.request("tools/list", json!({}))?;
        Ok(parse_tools(&result))
    }

    pub fn call_tool(
        &mut self,
        tool: &str,
        arguments: Value,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
        parse_tool_call_result(&result).map_err(|error| {
            format!(
                "MCP tool `{}` failed: {}",
                sanitize_external_text(tool),
                sanitize_external_text(&error)
            )
            .into()
        })
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), Box<dyn std::error::Error>> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_line(&message, Instant::now() + self.timeout)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let deadline = Instant::now() + self.timeout;
        self.write_line(&message, deadline)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let error = format!(
                    "MCP server `{}` timed out after {} seconds on `{method}`",
                    sanitize_external_text(&self.name),
                    self.timeout.as_secs()
                );
                self.terminate();
                return Err(error.into());
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(line) => line,
                Err(_) => {
                    let error = format!(
                        "MCP server `{}` closed or timed out during `{method}`",
                        sanitize_external_text(&self.name)
                    );
                    self.terminate();
                    return Err(error.into());
                }
            };
            let line = match line {
                Ok(line) => line,
                Err(source) => {
                    let error = format!(
                        "MCP server `{}` sent an invalid response: {error}",
                        sanitize_external_text(&self.name),
                        error = sanitize_external_text(&source)
                    );
                    self.terminate();
                    return Err(error.into());
                }
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                // Tolerate stray non-JSON output instead of failing the call.
                continue;
            };
            if value["id"].as_u64() != Some(id) {
                continue; // Server-initiated notifications are ignored.
            }
            if let Some(error) = value.get("error") {
                let message =
                    sanitize_external_text(error["message"].as_str().unwrap_or("unknown error"));
                return Err(format!(
                    "MCP server `{}`: {message}",
                    sanitize_external_text(&self.name)
                )
                .into());
            }
            return Ok(value["result"].clone());
        }
    }

    fn write_line(
        &mut self,
        message: &Value,
        deadline: Instant,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.terminated {
            return Err("MCP server is no longer running after an earlier timeout.".into());
        }
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        let writer = Arc::clone(&self.writer);
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let outcome = writer
                .lock()
                .map_err(|_| "MCP stdin lock failed".to_owned())
                .and_then(|mut writer| {
                    writer.write_all(&line).map_err(|error| error.to_string())?;
                    writer.flush().map_err(|error| error.to_string())
                });
            let _ = sender.send(outcome);
        });
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                let message = format!(
                    "MCP server `{}` write failed: {}",
                    sanitize_external_text(&self.name),
                    sanitize_external_text(&error)
                );
                self.terminate();
                Err(message.into())
            }
            Err(_) => {
                self.terminate();
                Err(format!(
                    "MCP server `{}` timed out while receiving a request",
                    sanitize_external_text(&self.name)
                )
                .into())
            }
        }
    }

    /// End the server and everything it spawned. The child is its own
    /// process-group leader, so signalling the negative group id (before the
    /// leader is reaped and its pid freed) reaches shell-spawned descendants
    /// that would otherwise keep the stdio pipes open and the detached
    /// reader/writer threads blocked. Used by both the timeout path and drop.
    fn terminate(&mut self) {
        self.terminated = true;
        // `--` is required: without it a negative group id parses as an
        // option list instead of a target.
        let _ = Command::new("/bin/kill")
            .args(["-9", "--", &format!("-{}", self.child.id())])
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read one newline-delimited MCP message while retaining at most the fixed
/// line budget. Oversized lines are drained without buffering, then rejected.
fn read_bounded_line(reader: &mut impl Read) -> Result<Option<String>, std::io::Error> {
    let mut bytes = Vec::with_capacity(1_024);
    let mut one = [0_u8; 1];
    let mut overflow = false;
    loop {
        match reader.read(&mut one)? {
            0 if bytes.is_empty() && !overflow => return Ok(None),
            0 => break,
            _ if one[0] == b'\n' => break,
            _ if bytes.len() < MAX_MCP_LINE_BYTES => bytes.push(one[0]),
            _ => overflow = true,
        }
    }
    if overflow {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response line exceeds 64 KiB",
        ));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn parse_tools(result: &Value) -> Vec<McpToolInfo> {
    result["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    Some(McpToolInfo {
                        name: sanitize_external_text(tool["name"].as_str()?),
                        description: sanitize_external_text(
                            tool["description"].as_str().unwrap_or_default(),
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_tool_call_result(result: &Value) -> Result<String, String> {
    let mut text = String::new();
    if let Some(content) = result["content"].as_array() {
        for part in content {
            match part["type"].as_str() {
                Some("text") => {
                    bounded_append(&mut text, part["text"].as_str().unwrap_or_default())
                }
                Some(other) => bounded_append(&mut text, &format!("[{other} content omitted]")),
                None => {}
            }
        }
    }
    if result["isError"].as_bool() == Some(true) {
        return Err(if text.is_empty() {
            "the server reported an error".into()
        } else {
            sanitize_external_text(&text)
        });
    }
    Ok(sanitize_external_text(&text))
}

fn bounded_append(target: &mut String, value: &str) {
    if target.ends_with(TRUNCATION_NOTICE) {
        return;
    }
    let limit = MAX_RESULT_CHARS.saturating_sub(TRUNCATION_NOTICE.chars().count());
    if target.chars().count() >= limit {
        target.push_str(TRUNCATION_NOTICE);
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    let remaining = limit.saturating_sub(target.chars().count());
    target.extend(value.chars().take(remaining));
    if value.chars().count() > remaining {
        target.push_str(TRUNCATION_NOTICE);
    }
}

/// External output is untrusted: strip terminal control sequences and bound
/// its size before it is displayed anywhere. Links stay visible as plain
/// text; MICE never follows them on its own.
pub fn sanitize_external_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(MAX_RESULT_CHARS));
    let mut in_escape = false;
    let limit = MAX_RESULT_CHARS.saturating_sub(TRUNCATION_NOTICE.chars().count());
    for character in value.chars() {
        if sanitized.chars().count() >= limit {
            sanitized.push_str(TRUNCATION_NOTICE);
            break;
        }
        if in_escape {
            if character.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        match character {
            '\u{1b}' => in_escape = true,
            '\n' | '\t' => sanitized.push(character),
            character if character.is_control() => {}
            character => sanitized.push(character),
        }
    }
    sanitized
}

/// The enabled, well-formed server entries — the only ones MICE will spawn.
pub fn granted_servers(config: &mice_core::Config) -> Vec<&McpServerConfig> {
    let mut seen = std::collections::BTreeSet::new();
    config
        .mcp
        .servers
        .iter()
        .filter(|server| {
            server.enabled
                && !server.name.trim().is_empty()
                && !server.command.trim().is_empty()
                && seen.insert(server.name.as_str())
        })
        .collect()
}

/// A crude search-tool detector for the overlay's Fetch Links action.
pub fn is_search_tool(tool: &McpToolInfo) -> bool {
    let haystack = format!("{} {}", tool.name, tool.description).to_ascii_lowercase();
    haystack.contains("search") || haystack.contains("web") || haystack.contains("lookup")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A scripted /bin/sh MCP server: enough protocol for a full network-free
    /// spawn → initialize → list → call round trip.
    fn mock_server_script() -> String {
        [
            r#"read line; printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock"}}}\n'"#,
            "read line",
            r#"read line; printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"web_search","description":"Search the web"}]}}\n'"#,
            r#"read line; printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"1. Example — https://example.com"}]}}\n'"#,
        ]
        .join("; ")
    }

    fn mock_config(script: String) -> McpServerConfig {
        McpServerConfig {
            name: "mock".into(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            enabled: true,
        }
    }

    #[test]
    fn spawns_handshakes_lists_and_calls_a_stdio_server() {
        let mut server = McpServerProcess::spawn(&mock_config(mock_server_script())).unwrap();
        let tools = server.list_tools().unwrap();
        assert_eq!(
            tools,
            vec![McpToolInfo {
                name: "web_search".into(),
                description: "Search the web".into(),
            }]
        );
        assert!(is_search_tool(&tools[0]));
        let answer = server
            .call_tool("web_search", json!({"query": "example"}))
            .unwrap();
        assert_eq!(answer, "1. Example — https://example.com");
    }

    #[test]
    fn timeout_cleanup_kills_the_whole_process_tree() {
        let pid_file = std::env::temp_dir().join(format!(
            "mice-mcp-tree-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        // The shell spawns a background grandchild, records its pid, then
        // goes silent so initialize times out.
        let script = format!("sleep 30 & echo $! > '{}'; read line", pid_file.display());
        let error =
            McpServerProcess::spawn_with_timeout(&mock_config(script), Duration::from_millis(300))
                .err();
        assert!(error.is_some());
        thread::sleep(Duration::from_millis(200));
        let grandchild = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .to_owned();
        assert!(!grandchild.is_empty(), "grandchild pid was not recorded");
        // `ps -p` succeeds only for a live process; `kill -0` would also
        // fail with EPERM for a live process we cannot signal, which must
        // not count as proof of death.
        let alive = Command::new("/bin/ps")
            .args(["-p", &grandchild])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(!alive, "grandchild process survived MCP cleanup");
        let _ = std::fs::remove_file(&pid_file);
    }

    #[test]
    fn a_silent_server_times_out_instead_of_hanging_mice() {
        let config = mock_config("read line; sleep 30".into());
        let started = Instant::now();
        let error = McpServerProcess::spawn_with_timeout(&config, Duration::from_millis(200))
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(
            error.contains("timed out") || error.contains("closed"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_read_timeout_terminates_a_retained_server() {
        let config = mock_config(
            [
                r#"read line; printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}\n'"#,
                "read line; sleep 30",
            ]
            .join("; "),
        );
        let mut server =
            McpServerProcess::spawn_with_timeout(&config, Duration::from_millis(200)).unwrap();
        let error = server.list_tools().unwrap_err().to_string();
        assert!(
            error.contains("timed out") || error.contains("closed"),
            "{error}"
        );
        assert!(server.terminated);
        assert!(server.child.try_wait().unwrap().is_some());
    }

    #[test]
    fn tool_errors_and_non_text_content_are_reported_safely() {
        let error = parse_tool_call_result(&json!({
            "isError": true,
            "content": [{"type": "text", "text": "query too long"}],
        }))
        .unwrap_err();
        assert_eq!(error, "query too long");
        let mixed = parse_tool_call_result(&json!({
            "content": [
                {"type": "text", "text": "answer"},
                {"type": "image", "data": "…"},
            ],
        }))
        .unwrap();
        assert_eq!(mixed, "answer\n[image content omitted]");
    }

    #[test]
    fn external_text_is_stripped_of_control_sequences_and_bounded() {
        let sanitized = sanitize_external_text("safe \u{1b}[31mred\u{1b}[0m text\u{7}");
        assert_eq!(sanitized, "safe red text");
        let long = sanitize_external_text(&"a".repeat(20_000));
        assert!(long.chars().count() <= MAX_RESULT_CHARS);
        assert!(long.ends_with("(external result truncated)"));
    }

    #[test]
    fn oversized_mcp_lines_are_rejected_before_json_parsing() {
        let mut bytes = vec![b'x'; MAX_MCP_LINE_BYTES + 1];
        bytes.push(b'\n');
        let error = read_bounded_line(&mut Cursor::new(bytes)).unwrap_err();
        assert!(error.to_string().contains("64 KiB"));
    }

    #[test]
    fn imported_tool_names_and_errors_are_sanitized_and_bounded() {
        let tools = parse_tools(&json!({
            "tools": [{"name": "bad\u{001b}[31mtool", "description": "bad\u{0007} description"}]
        }));
        assert_eq!(tools[0].name, "badtool");
        assert_eq!(tools[0].description, "bad description");
        let answer = parse_tool_call_result(&json!({
            "content": [{"type": "text", "text": "x\u{001b}[2J".repeat(10_000)}]
        }))
        .unwrap();
        assert!(!answer.contains('\u{1b}'));
        assert!(answer.chars().count() <= MAX_RESULT_CHARS);
    }

    #[test]
    fn only_enabled_wellformed_servers_are_granted() {
        let mut config = mice_core::Config::default();
        config.mcp.servers = vec![
            McpServerConfig {
                name: "search".into(),
                command: "/bin/true".into(),
                args: vec![],
                enabled: true,
            },
            McpServerConfig {
                name: "disabled".into(),
                command: "/bin/true".into(),
                args: vec![],
                enabled: false,
            },
            McpServerConfig {
                name: "".into(),
                command: "/bin/true".into(),
                args: vec![],
                enabled: true,
            },
            McpServerConfig {
                name: "search".into(),
                command: "/bin/other".into(),
                args: vec![],
                enabled: true,
            },
        ];
        let granted = granted_servers(&config);
        assert_eq!(granted.len(), 1);
        assert_eq!(granted[0].command, "/bin/true");
    }
}
