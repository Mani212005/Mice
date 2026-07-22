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
    /// Whether the OS has granted global input observation. Defaults so older
    /// agents that predate this field still deserialize.
    #[serde(default)]
    pub input_monitoring: bool,
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
    /// The currently active native application, captured once with the AX
    /// node. It lets the core turn generic platform labels such as Terminal's
    /// “shell text field” into an accurate user-facing explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
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
    /// An explicit definition request. Unlike the summarize gesture it never
    /// infers intent from selection length.
    Define,
    Image,
}

/// Pasteboard representations captured after the user's own Cmd-C, sent only
/// when the explicit smart-copy gesture fires. The agent never observes the
/// pasteboard continuously; the core decides whether anything can be enriched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardCaptured {
    pub session_id: String,
    /// The platform could not safely send the captured representations inside
    /// the bounded IPC frame. The core must leave the pasteboard untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtf_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub png_base64: Option<String>,
}

/// The scope of a native screen capture the core may request. The agent owns
/// the actual capture, permissions, and display/window resolution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenCaptureScope {
    /// The frontmost window of the frontmost application.
    FrontWindow,
    /// The entire display currently under the mouse pointer.
    DisplayUnderMouse,
    /// The frontmost window captured at native pixel resolution with tiled
    /// OCR, for dense small text such as spreadsheets. The image sent onward
    /// remains bounded; only the on-device OCR pass sees full resolution.
    FrontWindowDetail,
}

/// A native screen capture produced only in response to an explicit
/// `screen.capture` request. Captures are never persisted; `capture_error`
/// reports a refusal (missing permission, sensitive app) without breaking
/// the IPC stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScreenCaptured {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub png_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
}

/// The current Finder selection, read only after an explicit filing command.
/// File paths are transient and are never persisted by the platform agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FinderCaptured {
    pub session_id: String,
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
}

/// Text submitted by a native prompt owned by the platform agent. The core
/// retains the session state and decides how a submission is interpreted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PromptSubmitted {
    pub session_id: String,
    pub text: String,
}

/// Text submitted through MICE's daemon-only command palette. Selection and
/// front-app context are captured once at submit time, never observed later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PaletteSubmitted {
    pub session_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub front_app_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_text: Option<String>,
}

/// Escape dismisses exactly one palette request. The core treats this as an
/// explicit cancellation boundary, so a late response can never affect a
/// newer palette session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PaletteDismissed {
    pub session_id: String,
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
    /// A native, keyboard-accessible reference surface. Unlike a result
    /// overlay it is intentionally centered and remains visible until the
    /// person dismisses it, so first-run help is never mistaken for output.
    HomeShow {
        text: String,
    },
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
    PaletteShow {
        session_id: String,
        prefill: Option<String>,
    },
    PaletteAppendResult {
        session_id: String,
        chunk: String,
    },
    PaletteFinishResult {
        session_id: String,
        text: Option<String>,
    },
    /// Hide the palette for one specific session, so a stale request that
    /// finishes late can never dismiss a newer palette presentation.
    PaletteDismiss {
        session_id: String,
    },
    OverlayGuideStep {
        session_id: String,
        step_index: usize,
        total_steps: usize,
        instruction: String,
        app_hint: String,
        sensitive: bool,
        browser_capable: bool,
        /// `panel` opts into MICE's non-blocking guide surface. Absent keeps
        /// older platform agents compatible with the original native alert.
        presentation: Option<String>,
    },
    /// Ask the platform agent for one native screen capture. The agent
    /// answers with a `screen.captured` notification carrying the same
    /// session ID (or a typed refusal).
    ScreenCapture {
        session_id: String,
        scope: ScreenCaptureScope,
    },
    /// Read the Finder's current selection. This is read-only and is issued
    /// only by `mice file --finder` after the user asks for it.
    FinderCapture {
        session_id: String,
    },
    ClipboardSet {
        contents: ClipboardContents,
    },
    /// Paste the already-prepared clipboard contents into the app that remains
    /// frontmost behind MICE's non-activating overlay panel.
    ClipboardPaste,
    OverlayDismiss,
    AgentStop,
}

impl AgentCommand {
    pub fn notification(&self) -> RpcNotification {
        let (method, params) = match self {
            Self::HomeShow { text } => ("home.show", json!({ "text": text })),
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
                presentation,
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
                    "presentation": presentation,
                }),
            ),
            Self::PaletteShow {
                session_id,
                prefill,
            } => (
                "palette.show",
                json!({ "sessionId": session_id, "prefill": prefill }),
            ),
            Self::PaletteAppendResult { session_id, chunk } => (
                "palette.result.append",
                json!({ "sessionId": session_id, "chunk": chunk }),
            ),
            Self::PaletteFinishResult { session_id, text } => (
                "palette.result.finish",
                json!({ "sessionId": session_id, "text": text }),
            ),
            Self::PaletteDismiss { session_id } => {
                ("palette.hide", json!({ "sessionId": session_id }))
            }
            Self::ScreenCapture { session_id, scope } => (
                "screen.capture",
                json!({ "sessionId": session_id, "scope": scope }),
            ),
            Self::FinderCapture { session_id } => {
                ("finder.capture", json!({ "sessionId": session_id }))
            }
            Self::ClipboardSet { contents } => (
                "clipboard.set",
                json!({
                    "text": contents.text,
                    "html": contents.html,
                    "rtf": contents.rtf,
                    "pngBase64": contents.png_base64,
                }),
            ),
            Self::ClipboardPaste => ("clipboard.paste", json!({})),
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

        assert_eq!(
            AgentCommand::ClipboardPaste.notification(),
            RpcNotification {
                jsonrpc: "2.0".into(),
                method: "clipboard.paste".into(),
                params: json!({}),
            }
        );
    }

    #[test]
    fn home_surface_uses_the_shared_agent_protocol() {
        assert_eq!(
            AgentCommand::HomeShow {
                text: "MICE Home".into(),
            }
            .notification(),
            RpcNotification {
                jsonrpc: "2.0".into(),
                method: "home.show".into(),
                params: json!({"text": "MICE Home"}),
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
    fn clipboard_captured_omits_absent_representations() {
        let captured = ClipboardCaptured {
            session_id: "smart-copy-1".into(),
            capture_error: None,
            text: Some("Name\tScore".into()),
            html: Some("<table><tr><td>Name</td></tr></table>".into()),
            rtf_base64: None,
            png_base64: None,
        };
        assert_eq!(
            serde_json::to_value(&captured).unwrap(),
            json!({
                "sessionId": "smart-copy-1",
                "text": "Name\tScore",
                "html": "<table><tr><td>Name</td></tr></table>",
            })
        );
        let round_trip: ClipboardCaptured =
            serde_json::from_value(json!({"sessionId": "smart-copy-1"})).unwrap();
        assert_eq!(round_trip.session_id, "smart-copy-1");
        assert!(round_trip.text.is_none());
    }

    #[test]
    fn clipboard_captured_reports_a_bounded_capture_failure() {
        let captured = ClipboardCaptured {
            session_id: "smart-copy-1".into(),
            capture_error: Some("Copied content is too large".into()),
            text: None,
            html: None,
            rtf_base64: None,
            png_base64: None,
        };
        assert_eq!(
            serde_json::to_value(captured).unwrap(),
            json!({
                "sessionId": "smart-copy-1",
                "captureError": "Copied content is too large"
            })
        );
    }

    #[test]
    fn screen_capture_request_and_reply_share_a_stable_wire_shape() {
        let command = AgentCommand::ScreenCapture {
            session_id: "see-1".into(),
            scope: ScreenCaptureScope::FrontWindow,
        };
        assert_eq!(
            command.notification(),
            RpcNotification {
                jsonrpc: "2.0".into(),
                method: "screen.capture".into(),
                params: json!({ "sessionId": "see-1", "scope": "front_window" }),
            }
        );
        let refusal = ScreenCaptured {
            session_id: "see-1".into(),
            capture_error: Some("Screen Recording permission is not granted".into()),
            png_base64: None,
            ocr_text: None,
            app_name: None,
            window_title: None,
        };
        assert_eq!(
            serde_json::to_value(refusal).unwrap(),
            json!({
                "sessionId": "see-1",
                "captureError": "Screen Recording permission is not granted",
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

    #[test]
    fn palette_messages_have_distinct_stable_shapes() {
        let submitted = PaletteSubmitted {
            session_id: "palette-1".into(),
            text: "summarize".into(),
            front_app_name: Some("Notes".into()),
            selection_text: Some("Selected text".into()),
        };
        assert_eq!(
            serde_json::to_value(submitted).unwrap(),
            json!({"sessionId":"palette-1", "text":"summarize", "frontAppName":"Notes", "selectionText":"Selected text"})
        );
        assert_eq!(
            AgentCommand::PaletteShow {
                session_id: "palette-1".into(),
                prefill: Some("plan ".into())
            }
            .notification()
            .method,
            "palette.show"
        );
        assert_eq!(
            AgentCommand::PaletteAppendResult {
                session_id: "palette-1".into(),
                chunk: "hi".into()
            }
            .notification()
            .method,
            "palette.result.append"
        );
        let dismiss = AgentCommand::PaletteDismiss {
            session_id: "palette-1".into(),
        }
        .notification();
        assert_eq!(dismiss.method, "palette.hide");
        assert_eq!(dismiss.params["sessionId"], "palette-1");
        assert_eq!(
            serde_json::to_value(PaletteDismissed {
                session_id: "palette-1".into(),
            })
            .unwrap(),
            json!({"sessionId": "palette-1"})
        );
    }

    #[test]
    fn guide_panel_presentation_is_opt_in_and_on_the_shared_wire() {
        let notification = AgentCommand::OverlayGuideStep {
            session_id: "goal-1".into(),
            step_index: 0,
            total_steps: 3,
            instruction: "Open Notes.".into(),
            app_hint: "Notes".into(),
            sensitive: false,
            browser_capable: false,
            presentation: Some("panel".into()),
        }
        .notification();
        assert_eq!(notification.method, "overlay.guideStep");
        assert_eq!(notification.params["presentation"], "panel");
    }
}
