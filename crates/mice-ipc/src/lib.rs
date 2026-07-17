//! Versioned, length-prefixed JSON-RPC messages used by the core and platform agent.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "1.0";
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub screen_capture: bool,
    pub ax_read: bool,
    pub inject_text: bool,
    pub overlay: bool,
    pub local_ocr: bool,
    pub browser_bridge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub platform: String,
    pub capabilities: Capabilities,
}

/// A platform-neutral rectangle in global screen coordinates. The platform
/// agent owns coordinate conversion before sending this across IPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AxSnapshot {
    pub role: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub actions: Vec<String>,
}

/// AX-first hover context. `pixels` is optional because an accessibility-rich
/// target should not require a screen capture before the core can route it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HoverCaptured {
    pub session_id: String,
    pub ax: Option<AxSnapshot>,
    pub text: Option<String>,
    pub pixels: Option<String>,
    pub bounds: Option<ScreenRect>,
}

/// Text selected with the host application's normal controls. The platform
/// agent emits this only after a configured MICE action gesture; it never
/// owns a mouse drag or click.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SelectionText {
    pub session_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    pub source: SelectionSource,
    pub action: SelectionAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    Ax,
    Clipboard,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionAction {
    Summarize,
    Image,
}

/// Text submitted by a native prompt owned by the platform agent. The core
/// retains the session state and decides how a submission is interpreted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PromptSubmitted {
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HighlightBox {
    pub bounds: ScreenRect,
    pub instruction_text: String,
}

/// A button shown on the interactive result panel. `id` is echoed back to the
/// core in an `overlay.action` notification when the button is pressed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayAction {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
}

/// Commands emitted by the portable core for the platform agent to render or
/// perform. The method names and wire parameters live here so callers do not
/// re-declare protocol shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    OverlayShow {
        text: String,
    },
    /// Updates a visible narration panel without moving it to the current
    /// mouse position. This keeps multi-step guidance visually calm.
    OverlayUpdate {
        text: String,
    },
    OverlayAppendResult {
        chunk: String,
    },
    OverlayFinishResult {
        text: Option<String>,
    },
    /// Declares which action buttons the result panel should show and the
    /// session to echo back when one is pressed. Sent after a result finishes.
    OverlayResult {
        session_id: String,
        actions: Vec<OverlayAction>,
    },
    OverlayShowImage {
        png_base64: String,
    },
    OverlayHighlight {
        boxes: Vec<HighlightBox>,
    },
    OverlayPromptInput {
        session_id: String,
        title: String,
        placeholder: String,
        context: Option<String>,
    },
    OverlayGuideStep {
        session_id: String,
        step_index: usize,
        total_steps: usize,
        instruction: String,
        app_hint: String,
        sensitive: bool,
        browser_capable: bool,
    },
    ClipboardSet {
        contents: ClipboardContents,
    },
    OverlayDismiss,
    AgentStop,
}

impl AgentCommand {
    pub fn notification(&self) -> RpcNotification {
        let (method, params) = match self {
            Self::OverlayShow { text } => ("overlay.show", json!({ "text": text })),
            Self::OverlayUpdate { text } => ("overlay.update", json!({ "text": text })),
            Self::OverlayAppendResult { chunk } => {
                ("overlay.appendResult", json!({ "chunk": chunk }))
            }
            Self::OverlayFinishResult { text: Some(text) } => {
                ("overlay.finishResult", json!({ "text": text }))
            }
            Self::OverlayFinishResult { text: None } => ("overlay.finishResult", json!({})),
            Self::OverlayResult {
                session_id,
                actions,
            } => (
                "overlay.result",
                json!({ "sessionId": session_id, "actions": actions }),
            ),
            Self::OverlayShowImage { png_base64 } => {
                ("overlay.showImage", json!({ "pngBase64": png_base64 }))
            }
            Self::OverlayHighlight { boxes } => ("overlay.highlight", json!({ "boxes": boxes })),
            Self::OverlayPromptInput {
                session_id,
                title,
                placeholder,
                context,
            } => (
                "overlay.promptInput",
                json!({
                    "sessionId": session_id,
                    "title": title,
                    "placeholder": placeholder,
                    "context": context,
                }),
            ),
            Self::OverlayGuideStep {
                session_id,
                step_index,
                total_steps,
                instruction,
                app_hint,
                sensitive,
                browser_capable,
            } => (
                "overlay.guideStep",
                json!({
                    "sessionId": session_id,
                    "stepIndex": step_index,
                    "totalSteps": total_steps,
                    "instruction": instruction,
                    "appHint": app_hint,
                    "sensitive": sensitive,
                    "browserCapable": browser_capable,
                }),
            ),
            Self::ClipboardSet { contents } => (
                "clipboard.set",
                json!({
                    "text": contents.text,
                    "html": contents.html,
                    "rtf": contents.rtf,
                    "pngBase64": contents.png_base64,
                }),
            ),
            Self::OverlayDismiss => ("overlay.dismiss", json!({})),
            Self::AgentStop => ("agent.stop", json!({})),
        };
        RpcNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

/// Formats sent from the portable clipboard engine to the native agent. PNG is
/// deliberately optional and will be added by the image action without
/// changing the IPC method shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardContents {
    pub text: String,
    pub html: String,
    pub rtf: String,
    pub png_base64: Option<String>,
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("truncated frame")]
    Truncated,
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> Result<T, FrameError> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::Truncated);
        }
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            error.into()
        }
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trip() {
        let message = RpcNotification {
            jsonrpc: "2.0".into(),
            method: "ping".into(),
            params: serde_json::json!({"n": 1}),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &message).unwrap();
        assert_eq!(
            read_frame::<RpcNotification>(&mut bytes.as_slice()).unwrap(),
            message
        );
    }

    #[test]
    fn agent_commands_have_stable_json_rpc_shapes() {
        let command = AgentCommand::ClipboardSet {
            contents: ClipboardContents {
                text: "summary".into(),
                html: "<p>summary</p>".into(),
                rtf: "{\\rtf1 summary}".into(),
                png_base64: None,
            },
        };
        assert_eq!(
            command.notification(),
            RpcNotification {
                jsonrpc: "2.0".into(),
                method: "clipboard.set".into(),
                params: json!({"text": "summary", "html": "<p>summary</p>", "rtf": "{\\rtf1 summary}", "pngBase64": null}),
            }
        );
    }

    #[test]
    fn overlay_result_declares_session_and_action_buttons() {
        let command = AgentCommand::OverlayResult {
            session_id: "sel-1".into(),
            actions: vec![OverlayAction {
                id: "go_deeper".into(),
                label: "Go Deeper".into(),
            }],
        };
        assert_eq!(
            command.notification(),
            RpcNotification {
                jsonrpc: "2.0".into(),
                method: "overlay.result".into(),
                params: json!({
                    "sessionId": "sel-1",
                    "actions": [{"id": "go_deeper", "label": "Go Deeper"}],
                }),
            }
        );
    }

    #[test]
    fn guide_highlight_uses_shared_screen_coordinates() {
        let command = AgentCommand::OverlayHighlight {
            boxes: vec![HighlightBox {
                bounds: ScreenRect {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 40.0,
                },
                instruction_text: "Open Settings".into(),
            }],
        };
        assert_eq!(
            command.notification().params,
            json!({"boxes": [{
                "bounds": {"x": 10.0, "y": 20.0, "width": 30.0, "height": 40.0},
                "instructionText": "Open Settings"
            }]})
        );
    }

    #[test]
    fn selected_text_uses_stable_typed_actions() {
        let selection = SelectionText {
            session_id: "selection-1".into(),
            text: "A selected paragraph".into(),
            html: Some("<p>A selected paragraph</p>".into()),
            source: SelectionSource::Clipboard,
            action: SelectionAction::Summarize,
        };
        assert_eq!(
            serde_json::to_value(selection).unwrap(),
            json!({
                "sessionId": "selection-1",
                "text": "A selected paragraph",
                "html": "<p>A selected paragraph</p>",
                "source": "clipboard",
                "action": "summarize"
            })
        );
    }

    #[test]
    fn prompt_input_uses_a_shared_session_id() {
        let command = AgentCommand::OverlayPromptInput {
            session_id: "goal-1".into(),
            title: "What is your goal today?".into(),
            placeholder: "Describe a goal".into(),
            context: Some("MICE only guides.".into()),
        };
        assert_eq!(
            command.notification(),
            RpcNotification {
                jsonrpc: "2.0".into(),
                method: "overlay.promptInput".into(),
                params: json!({
                    "sessionId": "goal-1",
                    "title": "What is your goal today?",
                    "placeholder": "Describe a goal",
                    "context": "MICE only guides.",
                }),
            }
        );
    }
}
