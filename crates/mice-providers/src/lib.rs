//! Capability-based provider routing. Network transports are intentionally isolated
//! behind the provider clients so the router remains deterministic and testable.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_LOCAL_MODEL: &str = "gemma3:4b";
pub const DEFAULT_CLOUD_MODEL: &str = "gpt-5.6-luna";
/// Keep a daemon's local model resident without changing routing behavior.
pub const OLLAMA_KEEP_ALIVE: &str = "30m";
/// Fail quickly while establishing a connection, but never impose a total
/// deadline on a streaming generation. A healthy local model can legitimately
/// take longer than 45 seconds for a large summary or a goal plan; a total
/// `ureq` timeout would cut that response off mid-stream.
pub const OLLAMA_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn ollama_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(OLLAMA_CONNECT_TIMEOUT)
        .build()
}

/// Probes are non-streaming and must remain bounded. Keep their deadline
/// separate from the stream agent so startup checks cannot change generation
/// behavior.
fn ollama_probe_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(OLLAMA_CONNECT_TIMEOUT)
        .build()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    #[default]
    CloudAllowed,
    CloudOnly,
    LocalOnly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostPolicy {
    #[default]
    Cheapest,
    Fastest,
    BestQuality,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Locality {
    Local,
    Cloud,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Explain,
    Summarize,
    Rewrite,
    Translate,
    ExtractJson,
    Code,
    Image,
    Guide,
    GoalPlan,
    Qa,
    Define,
}

impl Action {
    /// Actions that can be fulfilled entirely from extracted text are the
    /// inexpensive local lane in M2. Pixel- and DOM-dependent work remains
    /// cloud-first unless privacy mode requires the local lane.
    pub fn prefers_local_text_lane(self) -> bool {
        matches!(
            self,
            Self::Explain
                | Self::Summarize
                | Self::Rewrite
                | Self::Translate
                | Self::ExtractJson
                | Self::Code
                | Self::Qa
                | Self::Define
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifacts {
    pub text: Option<String>,
    pub pixels: bool,
    pub ax: bool,
    pub dom: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPreferences {
    pub local_model: String,
    pub cloud_model: String,
}

impl Default for ModelPreferences {
    fn default() -> Self {
        Self {
            local_model: DEFAULT_LOCAL_MODEL.into(),
            cloud_model: DEFAULT_CLOUD_MODEL.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteRequest {
    pub artifacts: Artifacts,
    pub instruction: String,
    pub action: Option<Action>,
    pub privacy_mode: PrivacyMode,
    pub cost_policy: CostPolicy,
    #[serde(default)]
    pub model_preferences: ModelPreferences,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDescriptor {
    pub id: &'static str,
    pub locality: Locality,
    pub vision: bool,
    pub image_gen: bool,
    pub reasoning_tier: u8,
    pub speed_rank: u8,
    pub cost_rank: u8,
    /// Maximum estimated input tokens reserved for a single local request.
    /// Cloud models do not use this local-only budget.
    pub input_budget_tokens: Option<usize>,
    /// Ollama context window requested for a local model invocation.
    pub num_ctx: Option<usize>,
}

pub const MODELS: [ModelDescriptor; 9] = [
    ModelDescriptor {
        id: DEFAULT_LOCAL_MODEL,
        locality: Locality::Local,
        vision: true,
        image_gen: false,
        reasoning_tier: 2,
        speed_rank: 1,
        cost_rank: 0,
        input_budget_tokens: Some(12_000),
        num_ctx: Some(16_384),
    },
    ModelDescriptor {
        id: "phi4-mini",
        locality: Locality::Local,
        vision: false,
        image_gen: false,
        reasoning_tier: 1,
        speed_rank: 0,
        cost_rank: 0,
        input_budget_tokens: Some(6_000),
        num_ctx: Some(8_192),
    },
    ModelDescriptor {
        id: "gpt-oss:20b",
        locality: Locality::Local,
        vision: false,
        image_gen: false,
        reasoning_tier: 1,
        speed_rank: 2,
        cost_rank: 0,
        input_budget_tokens: Some(24_000),
        num_ctx: Some(32_768),
    },
    ModelDescriptor {
        id: DEFAULT_CLOUD_MODEL,
        locality: Locality::Cloud,
        vision: true,
        image_gen: false,
        reasoning_tier: 2,
        speed_rank: 0,
        cost_rank: 1,
        input_budget_tokens: None,
        num_ctx: None,
    },
    ModelDescriptor {
        id: "gpt-5.6-terra",
        locality: Locality::Cloud,
        vision: true,
        image_gen: false,
        reasoning_tier: 3,
        speed_rank: 1,
        cost_rank: 2,
        input_budget_tokens: None,
        num_ctx: None,
    },
    ModelDescriptor {
        id: "gpt-5.6-sol",
        locality: Locality::Cloud,
        vision: true,
        image_gen: false,
        reasoning_tier: 4,
        speed_rank: 2,
        cost_rank: 3,
        input_budget_tokens: None,
        num_ctx: None,
    },
    ModelDescriptor {
        id: "gpt-image-2",
        locality: Locality::Cloud,
        vision: true,
        image_gen: true,
        reasoning_tier: 0,
        speed_rank: 3,
        cost_rank: 2,
        input_budget_tokens: None,
        num_ctx: None,
    },
    ModelDescriptor {
        id: "llama-3.3-70b-versatile",
        locality: Locality::Cloud,
        vision: false,
        image_gen: false,
        reasoning_tier: 3,
        speed_rank: 0,
        cost_rank: 1,
        input_budget_tokens: None,
        num_ctx: None,
    },
    ModelDescriptor {
        id: "llama-3.1-8b-instant",
        locality: Locality::Cloud,
        vision: false,
        image_gen: false,
        reasoning_tier: 2,
        speed_rank: 0,
        cost_rank: 1,
        input_budget_tokens: None,
        num_ctx: None,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub model: ModelDescriptor,
    pub user_notice: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RouteError {
    #[error("This request needs cloud-only image generation, but privacy mode is local-only.")]
    CloudCapabilityBlocked,
    #[error("No provider satisfies this request.")]
    NoCandidate,
}

/// The execution shape selected for a text selection summary. Oversized
/// local-only inputs stay private, but need multiple bounded model calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionSummaryRoute {
    SingleShot(Route),
    Chunked { model: ModelDescriptor },
}

pub fn model_descriptor(id: &str) -> Option<&'static ModelDescriptor> {
    MODELS.iter().find(|model| model.id == id)
}

/// Select the large-input behavior for selection summaries without changing
/// routing for ordinary asks, hover explanations, or Goal Guide calls.
pub fn route_selection_summary(
    request: &RouteRequest,
    estimated_tokens: usize,
) -> Result<SelectionSummaryRoute, RouteError> {
    if request.privacy_mode == PrivacyMode::CloudOnly {
        return route(request).map(SelectionSummaryRoute::SingleShot);
    }

    let local = model_descriptor(&request.model_preferences.local_model)
        .filter(|model| model.locality == Locality::Local)
        .ok_or(RouteError::NoCandidate)?;
    let budget = local.input_budget_tokens.ok_or(RouteError::NoCandidate)?;
    if estimated_tokens <= budget {
        return route(request).map(SelectionSummaryRoute::SingleShot);
    }

    match request.privacy_mode {
        PrivacyMode::LocalOnly => Ok(SelectionSummaryRoute::Chunked {
            model: local.clone(),
        }),
        PrivacyMode::CloudAllowed => {
            let cloud = model_descriptor(&request.model_preferences.cloud_model)
                .filter(|model| model.locality == Locality::Cloud)
                .ok_or(RouteError::NoCandidate)?;
            Ok(SelectionSummaryRoute::SingleShot(Route {
                model: cloud.clone(),
                user_notice: Some(format!("Large selection — routed to {}", cloud.id)),
            }))
        }
        PrivacyMode::CloudOnly => unreachable!("handled above"),
    }
}

pub fn route(request: &RouteRequest) -> Result<Route, RouteError> {
    let needs_vision = request.artifacts.pixels && request.artifacts.text.is_none();
    let needs_image = request.action == Some(Action::Image);
    if request.privacy_mode == PrivacyMode::LocalOnly
        && (needs_image || request.action == Some(Action::Guide))
    {
        return Err(RouteError::CloudCapabilityBlocked);
    }
    let use_local_lane = match request.privacy_mode {
        PrivacyMode::LocalOnly => true,
        PrivacyMode::CloudOnly => false,
        // M2 keeps routine, text-only transformations private and inexpensive
        // by default. Vision, image generation, and guided/browser work remain
        // in the cloud lane when it is permitted.
        PrivacyMode::CloudAllowed => {
            request.artifacts.text.is_some()
                && !request.artifacts.pixels
                && request.action.is_some_and(Action::prefers_local_text_lane)
        }
    };
    let candidates = MODELS.iter().filter(|model| {
        (if use_local_lane {
            model.locality == Locality::Local
        } else {
            model.locality == Locality::Cloud
        }) && (!needs_vision || model.vision)
            && (!needs_image || model.image_gen)
    });
    let preferred_model = |model: &ModelDescriptor| match model.locality {
        Locality::Local => model.id == request.model_preferences.local_model,
        Locality::Cloud => model.id == request.model_preferences.cloud_model,
    };
    if request.action == Some(Action::Guide) {
        return MODELS
            .iter()
            .find(|model| model.id == "gpt-5.6-sol")
            .cloned()
            .map(|model| Route {
                model,
                user_notice: None,
            })
            .ok_or(RouteError::NoCandidate);
    }
    let model = match request.cost_policy {
        CostPolicy::Cheapest => {
            candidates.min_by_key(|model| (model.cost_rank, !preferred_model(model)))
        }
        CostPolicy::Fastest => {
            candidates.min_by_key(|model| (model.speed_rank, !preferred_model(model)))
        }
        CostPolicy::BestQuality => {
            candidates.max_by_key(|model| (model.reasoning_tier, preferred_model(model)))
        }
    }
    .ok_or(RouteError::NoCandidate)?;
    Ok(Route {
        model: model.clone(),
        user_notice: None,
    })
}

#[derive(Debug, Error)]
pub enum OllamaError {
    #[error("Ollama request failed: {0}")]
    Request(Box<ureq::Error>),
    #[error("Ollama response could not be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("Ollama returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Ollama failed: {0}")]
    Service(String),
    #[error("Could not present streamed model output: {0}")]
    Consumer(String),
}

fn ollama_request_error(error: ureq::Error) -> OllamaError {
    match error {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["error"].as_str().map(str::to_owned))
                .unwrap_or(body);
            let detail = detail.trim();
            if detail.is_empty() {
                OllamaError::Service(format!("Ollama returned HTTP {status}"))
            } else {
                OllamaError::Service(format!("Ollama returned HTTP {status}: {detail}"))
            }
        }
        error => OllamaError::Request(Box::new(error)),
    }
}

/// Build the Ollama request in the provider layer so the local transport is
/// testable without a running model server.
pub fn ollama_chat_payload(
    model: &str,
    instruction: &str,
    text: Option<&str>,
    num_ctx: usize,
    format: Option<serde_json::Value>,
) -> serde_json::Value {
    let content = match text {
        Some(text) => format!("{instruction}\n\nContent:\n{text}"),
        None => instruction.into(),
    };
    let mut payload = serde_json::json!({
        "model": model,
        "stream": true,
        "keep_alive": OLLAMA_KEEP_ALIVE,
        "messages": [{"role": "user", "content": content}],
        "options": {"num_ctx": num_ctx},
    });
    if let Some(format) = format {
        payload.as_object_mut().unwrap().insert("format".into(), format);
    }
    payload
}

/// Minimal non-streaming request for daemon startup. Failure is deliberately
/// non-fatal because cloud-only MICE does not require a local service.
pub fn ollama_warmup_payload(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "stream": false,
        "keep_alive": OLLAMA_KEEP_ALIVE,
        "messages": [{"role": "user", "content": "Reply with OK."}],
        "options": {"num_ctx": 256},
    })
}

pub fn warm_ollama_model(endpoint: &str, model: &str) -> Result<(), OllamaError> {
    ollama_probe_agent()
        .post(endpoint)
        .send_json(ollama_warmup_payload(model))
        .map_err(ollama_request_error)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct OllamaStreamEvent {
    #[serde(default)]
    message: Option<OllamaStreamMessage>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaStreamMessage {
    #[serde(default)]
    content: String,
}

/// Stream Ollama's `/api/chat` NDJSON response. The explicit endpoint keeps
/// all automated tests local and network-free.
pub fn stream_ollama_chat(
    endpoint: &str,
    model: &str,
    instruction: &str,
    text: Option<&str>,
    num_ctx: usize,
    format: Option<serde_json::Value>,
    mut on_chunk: impl FnMut(&str) -> Result<(), OllamaError>,
) -> Result<(), OllamaError> {
    let response = ollama_agent()
        .post(endpoint)
        .send_json(ollama_chat_payload(model, instruction, text, num_ctx, format))
        .map_err(ollama_request_error)?;
    for line in BufReader::new(response.into_reader()).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: OllamaStreamEvent = serde_json::from_str(&line)?;
        if let Some(error) = event.error {
            return Err(OllamaError::Service(error));
        }
        if let Some(message) = event.message.filter(|message| !message.content.is_empty()) {
            on_chunk(&message.content)?;
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

/// Retrieve an embedding from Ollama's `/api/embed` endpoint.
pub fn ollama_embed(
    endpoint: &str,
    model: &str,
    input: &str,
) -> Result<Vec<f32>, OllamaError> {
    let response = ollama_agent()
        .post(endpoint)
        .send_json(serde_json::json!({
            "model": model,
            "input": input,
            "keep_alive": OLLAMA_KEEP_ALIVE,
        }))
        .map_err(ollama_request_error)?;
    let body: OllamaEmbedResponse = serde_json::from_reader(response.into_reader())?;
    body.embeddings.into_iter().next().ok_or_else(|| {
        OllamaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No embeddings returned",
        ))
    })
}

/// Verify both the local Ollama service and a named model before selecting a
/// local tool-loop lane. A binary on PATH alone does not guarantee either.
pub fn ollama_model_ready(endpoint: &str, model: &str) -> Result<(), OllamaError> {
    let endpoint = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let response = ollama_probe_agent()
        .get(&endpoint)
        .call()
        .map_err(ollama_request_error)?;
    let body: serde_json::Value = serde_json::from_reader(response.into_reader())?;
    let installed = body["models"].as_array().is_some_and(|models| {
        models.iter().any(|entry| {
            entry["name"].as_str() == Some(model) || entry["model"].as_str() == Some(model)
        })
    });
    if installed {
        Ok(())
    } else {
        Err(OllamaError::Service(format!(
            "Ollama is running but model `{model}` is not installed"
        )))
    }
}

/// Strict structured output for the M6a planning/review flow. Plans are
/// advisory: the schema only permits textual instructions and a sensitivity
/// marker, never executable actions or selectors.
pub fn structured_goal_plan_payload(model: &str, goal: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": format!(
            "Create a practical step-by-step plan for this user goal:\n{goal}\n\nReturn 3 to 8 steps. MICE only guides: never claim to click, type, submit, log in, or handle credentials for the user. Mark any step involving personal data, logins, payments, or account setup as sensitive."
        ),
        "text": {"format": {
            "type": "json_schema",
            "name": "goal_plan",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["steps"],
                "properties": {
                    "steps": {
                        "type": "array",
                        "minItems": 3,
                        "maxItems": 8,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["instruction", "app_hint", "sensitive"],
                            "properties": {
                                "instruction": {"type": "string"},
                                "app_hint": {"type": "string"},
                                "sensitive": {"type": "boolean"}
                            }
                        }
                    }
                }
            }
        }}
    })
}

pub fn groq_goal_plan_payload(model: &str, goal: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "Return only JSON: {\"steps\":[{\"instruction\":string,\"app_hint\":string,\"sensitive\":boolean}]}. Give 3-8 practical advisory steps. MICE never clicks, types, submits forms, logs in, or handles credentials. Mark steps involving personal data, logins, payments, or account setup as sensitive."
            },
            {"role": "user", "content": format!("Create a plan for: {goal}")}
        ],
        "response_format": {"type": "json_object"},
        "temperature": 0
    })
}

/// Strict task-graph output used by Mission Control. The model proposes only
/// bounded metadata; the portable core validates every ID, dependency, and
/// path before a task can be assigned or launched.
pub fn structured_mission_plan_payload(model: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": prompt,
        "text": {"format": {
            "type": "json_schema",
            "name": "mission_task_graph",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["tasks"],
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 24,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["id", "title", "acceptance", "dependencies", "predicted_paths", "preferred_agent"],
                            "properties": {
                                "id": {"type": "string"},
                                "title": {"type": "string"},
                                "acceptance": {"type": "array", "minItems": 1, "items": {"type": "string"}},
                                "dependencies": {"type": "array", "items": {"type": "string"}},
                                "predicted_paths": {"type": "array", "items": {"type": "string"}},
                                "preferred_agent": {"type": ["string", "null"], "enum": ["codex", "claude", "antigravity", null]}
                            }
                        }
                    }
                }
            }
        }}
    })
}

pub fn groq_mission_plan_payload(model: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "Return exactly one JSON object matching the user's Mission Control task-graph contract. Never include Markdown or prose."},
            {"role": "user", "content": prompt}
        ],
        "response_format": {"type": "json_object"},
        "temperature": 0
    })
}

pub fn agent_loop_payload(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
) -> serde_json::Value {
    agent_loop_payload_with_image_and_persona(model, goal, observation, history, None, "patient")
}

/// OpenAI's Responses API accepts image parts alongside the compact DOM
/// observation. This is used only for sparse/canvas pages; normal turns stay
/// text-only and small.
pub fn agent_loop_payload_with_image(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    image_data_url: Option<&str>,
) -> serde_json::Value {
    agent_loop_payload_with_image_and_persona(
        model,
        goal,
        observation,
        history,
        image_data_url,
        "patient",
    )
}

pub fn agent_loop_payload_with_image_and_persona(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    image_data_url: Option<&str>,
    persona: &str,
) -> serde_json::Value {
    let mut content = vec![serde_json::json!({
        "type":"input_text",
        "text":format!("You are MICE, a careful browser helper. Your speaking style is {persona}. Goal: {goal}\n\nCurrent page observation:\n{observation}\n\nRecent action history:\n{history}\n\nChoose exactly one next action. Use supplied candidate_id values exactly; never invent selectors. Narrate in plain language. For the 'fill' action, you MUST put the text to type into the 'value' field. Prefer handoff rather than guessing. Never fill passwords, one-time codes, or payment fields; never click login, payment, purchase, transfer, final-submit, or file-return controls.")
    })];
    if let Some(image_data_url) = image_data_url {
        content.push(serde_json::json!({"type":"input_image", "image_url":image_data_url}));
    }
    serde_json::json!({
        "model": model,
        "input": [{"role":"user", "content":content}],
        "text": {"format": {"type":"json_schema", "name":"mice_agent_turn", "strict":true, "schema": {
            "type":"object", "additionalProperties":false,
            "required":["say_to_user","action","candidate_id","url","value","done_summary","question"],
            "properties": {
                "say_to_user":{"type":"string"},
                "action":{"type":"string","enum":["click","fill","open_url","scroll","done","handoff","ask_user"]},
                "candidate_id":{"type":["string","null"]}, "url":{"type":["string","null"]},
                "value":{"type":["string","null"]}, "done_summary":{"type":["string","null"]}, "question":{"type":["string","null"]}
            }
        }}}
    })
}

/// A one-shot "answer a question about this screen" vision request. The OCR
/// text rides along so the model can quote exact strings the image renders
/// small; the caller bounds both before building the payload.
pub fn openai_vision_answer_payload(
    model: &str,
    question: &str,
    ocr_text: &str,
    image_data_url: &str,
) -> serde_json::Value {
    let text = format!(
        "Answer the question about the attached screenshot of the user's own screen. Be concrete and concise; if the answer is not visible, say so plainly.\n\nQuestion: {question}\n\nText extracted from the screenshot by OCR (may contain errors):\n{ocr_text}"
    );
    serde_json::json!({
        "model": model,
        "input": [{"role":"user", "content":[
            {"type":"input_text", "text":text},
            {"type":"input_image", "image_url":image_data_url}
        ]}]
    })
}

/// Groq exposes JSON object mode through Chat Completions rather than the
/// Responses JSON-schema surface. The CLI still validates every field and
/// resolves candidate IDs locally before an action can reach the browser.
pub fn groq_agent_loop_payload(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
) -> serde_json::Value {
    groq_agent_loop_payload_with_persona(model, goal, observation, history, "patient")
}

pub fn ollama_agent_loop_payload_with_persona(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    image_data_url: Option<&str>,
    persona: &str,
) -> serde_json::Value {
    let mut user_msg = serde_json::json!({
        "role": "user",
        "content": format!("Goal: {goal}\n\nCurrent page observation:\n{observation}\n\nRecent action history:\n{history}")
    });
    if let Some(data_url) = image_data_url
        && let Some(idx) = data_url.find("base64,")
    {
        let base64_str = &data_url[idx + 7..];
        user_msg["images"] = serde_json::json!([base64_str]);
    }
    serde_json::json!({
        "model": model,
        "messages": [
            {"role":"system", "content":format!("You are MICE, a careful browser helper. Your speaking style is {persona}. Return only one JSON object with exactly these fields: say_to_user (string), action (click|fill|open_url|scroll|done|handoff|ask_user), candidate_id (string or null), url (string or null), value (string or null), done_summary (string or null), question (string or null). Choose exactly one action. For the 'fill' action, you MUST put the text to type into the 'value' field. Copy candidate_id exactly from the supplied candidates; never invent selectors. Narrate plainly and prefer handoff rather than guessing. Never fill passwords, one-time codes, or payment fields; never click login, payment, purchase, transfer, final-submit, or file-return controls.")},
            user_msg
        ],
        "format": "json",
        "stream": false,
        "options": {
            "temperature": 0
        }
    })
}

pub fn groq_agent_loop_payload_with_persona(
    model: &str,
    goal: &str,
    observation: &str,
    history: &str,
    persona: &str,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role":"system", "content":format!("You are MICE, a careful browser helper. Your speaking style is {persona}. Return only one JSON object with exactly these fields: say_to_user (string), action (click|fill|open_url|scroll|done|handoff|ask_user), candidate_id (string or null), url (string or null), value (string or null), done_summary (string or null), question (string or null). Choose exactly one action. For the 'fill' action, you MUST put the text to type into the 'value' field. Copy candidate_id exactly from the supplied candidates; never invent selectors. Narrate plainly and prefer handoff rather than guessing. Never fill passwords, one-time codes, or payment fields; never click login, payment, purchase, transfer, final-submit, or file-return controls.")},
            {"role":"user", "content":format!("Goal: {goal}\n\nCurrent page observation:\n{observation}\n\nRecent action history:\n{history}")}
        ],
        "response_format": {"type":"json_object"},
        "temperature": 0
    })
}

pub fn openai_responses_payload(
    model: &str,
    instruction: &str,
    text: Option<&str>,
) -> serde_json::Value {
    let mut content = vec![serde_json::json!({"type": "input_text", "text": instruction})];
    if let Some(text) = text {
        content.push(serde_json::json!({"type": "input_text", "text": text}));
    }
    serde_json::json!({"model": model, "stream": true, "input": [{"role": "user", "content": content}]})
}

/// GPT Image 2's Image API response contains the generated PNG in
/// `data[0].b64_json`. This payload deliberately uses a moderate square image
/// so it remains responsive and fits inside the IPC frame limit.
pub fn openai_image_generation_payload(prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "gpt-image-2",
        "prompt": prompt,
        "size": "1024x1024",
        "quality": "medium",
    })
}

pub fn openai_guide_payload(instruction: &str, dom_snapshot: &str) -> serde_json::Value {
    structured_guide_payload("gpt-5.6-sol", instruction, dom_snapshot)
}

/// OpenAI Responses-compatible strict structured output for browser guide-me.
/// Groq's Responses endpoint accepts this surface for models that support it.
pub fn structured_guide_payload(
    model: &str,
    instruction: &str,
    dom_snapshot: &str,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": format!("{instruction}\n\nVisible interactive elements (JSON):\n{dom_snapshot}"),
        "text": {"format": {
            "type": "json_schema",
            "name": "browser_guide_target",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["candidate_id", "instruction_text"],
                "properties": {
                    "candidate_id": {"type": "string"},
                    "instruction_text": {"type": "string"}
                }
            }
        }}
    })
}

/// Groq JSON Object Mode is supported across its chat-completions models. The
/// caller still validates both fields and resolves the chosen candidate ID to
/// the original selector before the browser receives it.
pub fn groq_guide_payload(model: &str, instruction: &str, dom_snapshot: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "Return only a JSON object with exactly these string fields: candidate_id and instruction_text. Copy candidate_id exactly from one supplied visible interactive element."
            },
            {
                "role": "user",
                "content": format!("{instruction}\n\nVisible interactive elements (JSON):\n{dom_snapshot}")
            }
        ],
        "response_format": {"type": "json_object"},
        "temperature": 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    #[test]
    fn vision_answer_payload_carries_question_ocr_and_image() {
        let payload = openai_vision_answer_payload(
            "gpt-5.6-sol",
            "What error is shown?",
            "Error: disk full",
            "data:image/png;base64,QUJD",
        );
        assert_eq!(payload["model"], "gpt-5.6-sol");
        let content = payload["input"][0]["content"].as_array().unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("What error is shown?"));
        assert!(text.contains("Error: disk full"));
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,QUJD");
    }

    fn summary_request(privacy_mode: PrivacyMode, preferences: ModelPreferences) -> RouteRequest {
        RouteRequest {
            artifacts: Artifacts {
                text: Some("selection".into()),
                ..Default::default()
            },
            instruction: "summarize".into(),
            action: Some(Action::Summarize),
            privacy_mode,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: preferences,
        }
    }

    #[test]
    fn large_cloud_allowed_selection_escalates_to_the_configured_cloud_model() {
        let request = summary_request(
            PrivacyMode::CloudAllowed,
            ModelPreferences {
                cloud_model: "gpt-5.6-terra".into(),
                ..Default::default()
            },
        );
        let SelectionSummaryRoute::SingleShot(route) =
            route_selection_summary(&request, 12_001).unwrap()
        else {
            panic!("large cloud-allowed selection should use the cloud lane");
        };
        assert_eq!(route.model.id, "gpt-5.6-terra");
        assert_eq!(
            route.user_notice.as_deref(),
            Some("Large selection — routed to gpt-5.6-terra")
        );
    }

    #[test]
    fn local_only_large_selection_chunks_and_heavy_model_stays_single_shot() {
        let request = summary_request(PrivacyMode::LocalOnly, ModelPreferences::default());
        assert!(matches!(
            route_selection_summary(&request, 12_001).unwrap(),
            SelectionSummaryRoute::Chunked { .. }
        ));

        let heavy_request = summary_request(
            PrivacyMode::LocalOnly,
            ModelPreferences {
                local_model: "gpt-oss:20b".into(),
                ..Default::default()
            },
        );
        let SelectionSummaryRoute::SingleShot(route) =
            route_selection_summary(&heavy_request, 20_000).unwrap()
        else {
            panic!("heavy local model should accept this selection in one request");
        };
        assert_eq!(route.model.id, "gpt-oss:20b");
    }

    #[test]
    fn large_local_only_explanation_uses_the_chunked_selection_route() {
        let mut request = summary_request(PrivacyMode::LocalOnly, ModelPreferences::default());
        request.action = Some(Action::Explain);
        request.instruction = "Explain this in depth".into();
        assert!(matches!(
            route_selection_summary(&request, 12_001).unwrap(),
            SelectionSummaryRoute::Chunked { model } if model.id == DEFAULT_LOCAL_MODEL
        ));
    }

    #[test]
    fn ollama_http_client_sends_context_budget_and_streams_ndjson() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1_024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= header_end + content_length {
                    request_tx
                        .send(request[header_end..header_end + content_length].to_vec())
                        .unwrap();
                    break;
                }
            }
            let body = concat!(
                "{\"message\":{\"content\":\"hello \"},\"done\":false}\n",
                "{\"message\":{\"content\":\"world\"},\"done\":false}\n",
                "{\"done\":true}\n"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let mut output = String::new();
        stream_ollama_chat(
            &endpoint,
            "gemma3:4b",
            "Summarize",
            Some("input"),
            16_384,
            None,
            |chunk| {
                output.push_str(chunk);
                Ok(())
            },
        )
        .unwrap();
        server.join().unwrap();

        let request: serde_json::Value =
            serde_json::from_slice(&request_rx.recv().unwrap()).unwrap();
        assert_eq!(request["model"], "gemma3:4b");
        assert_eq!(request["options"]["num_ctx"], 16_384);
        assert_eq!(
            request["messages"][0]["content"],
            "Summarize\n\nContent:\ninput"
        );
        assert_eq!(output, "hello world");
    }

    #[test]
    fn ollama_payloads_keep_the_daemon_model_warm() {
        let chat = ollama_chat_payload("gemma3:4b", "Summarize", Some("text"), 4096, None);
        assert_eq!(chat["keep_alive"], OLLAMA_KEEP_ALIVE);
        let warmup = ollama_warmup_payload("gemma3:4b");
        assert_eq!(warmup["model"], "gemma3:4b");
        assert_eq!(warmup["stream"], false);
        assert_eq!(warmup["keep_alive"], OLLAMA_KEEP_ALIVE);
        assert_eq!(warmup["options"]["num_ctx"], 256);
    }

    #[test]
    fn ollama_http_errors_include_the_server_message() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1_024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= header_end + content_length {
                    break;
                }
            }
            let body = r#"{"error":"model 'gemma3:4b' not found"}"#;
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let error = stream_ollama_chat(
            &endpoint,
            "gemma3:4b",
            "Summarize",
            Some("input"),
            16_384,
            None,
            |_| Ok(()),
        )
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            error.to_string(),
            "Ollama failed: Ollama returned HTTP 404: model 'gemma3:4b' not found"
        );
    }

    #[test]
    fn plain_text_uses_local_in_local_only_mode() {
        let request = RouteRequest {
            artifacts: Artifacts {
                text: Some("hello".into()),
                ..Default::default()
            },
            instruction: "summarize".into(),
            action: Some(Action::Summarize),
            privacy_mode: PrivacyMode::LocalOnly,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences::default(),
        };
        assert_eq!(route(&request).unwrap().model.id, "gemma3:4b");
    }

    #[test]
    fn image_generation_payload_uses_gpt_image_2() {
        assert_eq!(
            openai_image_generation_payload("Make an infographic"),
            serde_json::json!({
                "model": "gpt-image-2",
                "prompt": "Make an infographic",
                "size": "1024x1024",
                "quality": "medium",
            })
        );
    }

    #[test]
    fn guide_requests_use_sol_and_a_strict_schema() {
        let payload = openai_guide_payload("Find Settings", "[]");
        assert_eq!(payload["model"], "gpt-5.6-sol");
        assert_eq!(payload["text"]["format"]["strict"], true);
    }

    #[test]
    fn groq_guide_requests_use_json_object_mode() {
        let payload = groq_guide_payload("llama-3.3-70b-versatile", "Find Settings", "[]");
        assert_eq!(payload["model"], "llama-3.3-70b-versatile");
        assert_eq!(payload["response_format"]["type"], "json_object");
    }

    #[test]
    fn groq_agent_turn_uses_json_object_mode() {
        let payload =
            groq_agent_loop_payload("llama-3.3-70b-versatile", "Open Canva", "{}", "none");
        assert_eq!(payload["response_format"]["type"], "json_object");
        assert!(
            payload["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("candidate_id")
        );
    }

    #[test]
    fn agent_turn_can_attach_a_vision_observation() {
        let payload = agent_loop_payload_with_image(
            "gpt-5.6-sol",
            "Read this",
            "{}",
            "none",
            Some("data:image/jpeg;base64,abc"),
        );
        assert_eq!(payload["input"][0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn goal_plan_payload_is_strict_and_marks_sensitive_steps() {
        let payload = structured_goal_plan_payload("gpt-5.6-sol", "Open a bank account");
        assert_eq!(payload["text"]["format"]["strict"], true);
        assert_eq!(
            payload["text"]["format"]["schema"]["properties"]["steps"]["maxItems"],
            8
        );
        assert!(payload["input"].as_str().unwrap().contains("sensitive"));
    }

    #[test]
    fn mission_plan_payload_is_strict_and_bounded() {
        let payload = structured_mission_plan_payload("gpt-5.6-sol", "Make tasks");
        assert_eq!(payload["text"]["format"]["strict"], true);
        assert_eq!(
            payload["text"]["format"]["schema"]["properties"]["tasks"]["maxItems"],
            24
        );
        assert_eq!(
            payload["text"]["format"]["schema"]["properties"]["tasks"]["items"]["properties"]["preferred_agent"]
                ["enum"],
            serde_json::json!(["codex", "claude", "antigravity", null])
        );
        let groq = groq_mission_plan_payload("llama-3.3-70b-versatile", "Make tasks");
        assert_eq!(groq["response_format"]["type"], "json_object");
    }
    #[test]
    fn local_only_uses_gemma_for_vision() {
        let request = RouteRequest {
            artifacts: Artifacts {
                pixels: true,
                ..Default::default()
            },
            instruction: "explain".into(),
            action: Some(Action::Explain),
            privacy_mode: PrivacyMode::LocalOnly,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences::default(),
        };
        assert_eq!(route(&request).unwrap().model.id, "gemma3:4b");
    }

    #[test]
    fn cloud_allowed_uses_the_local_lane_for_text_actions() {
        let request = RouteRequest {
            artifacts: Artifacts {
                text: Some("hello".into()),
                ..Default::default()
            },
            instruction: "summarize".into(),
            action: Some(Action::Summarize),
            privacy_mode: PrivacyMode::CloudAllowed,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences::default(),
        };
        assert_eq!(route(&request).unwrap().model.id, DEFAULT_LOCAL_MODEL);
    }

    #[test]
    fn cloud_only_routes_text_actions_to_the_configured_cloud_model() {
        let request = RouteRequest {
            artifacts: Artifacts {
                text: Some("hello".into()),
                ..Default::default()
            },
            instruction: "summarize".into(),
            action: Some(Action::Summarize),
            privacy_mode: PrivacyMode::CloudOnly,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences {
                cloud_model: "llama-3.3-70b-versatile".into(),
                ..Default::default()
            },
        };
        assert_eq!(route(&request).unwrap().model.id, "llama-3.3-70b-versatile");
    }

    #[test]
    fn local_only_blocks_image_generation() {
        let request = RouteRequest {
            artifacts: Artifacts::default(),
            instruction: "make an infographic".into(),
            action: Some(Action::Image),
            privacy_mode: PrivacyMode::LocalOnly,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences::default(),
        };
        assert_eq!(route(&request), Err(RouteError::CloudCapabilityBlocked));
    }

    #[test]
    fn local_only_blocks_guide_requests() {
        let request = RouteRequest {
            artifacts: Artifacts::default(),
            instruction: "find settings".into(),
            action: Some(Action::Guide),
            privacy_mode: PrivacyMode::LocalOnly,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences::default(),
        };
        assert_eq!(route(&request), Err(RouteError::CloudCapabilityBlocked));
    }

    #[test]
    fn configured_local_model_breaks_local_cost_ties() {
        let request = RouteRequest {
            artifacts: Artifacts {
                text: Some("hello".into()),
                ..Default::default()
            },
            instruction: "summarize".into(),
            action: Some(Action::Summarize),
            privacy_mode: PrivacyMode::LocalOnly,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences {
                local_model: "phi4-mini".into(),
                ..Default::default()
            },
        };
        assert_eq!(route(&request).unwrap().model.id, "phi4-mini");
    }

    #[test]
    fn cloud_lane_uses_groq_when_preferred() {
        let request = RouteRequest {
            artifacts: Artifacts {
                text: Some("hello".into()),
                ..Default::default()
            },
            instruction: "general cloud request".into(),
            action: None,
            privacy_mode: PrivacyMode::CloudAllowed,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences {
                cloud_model: "llama-3.3-70b-versatile".into(),
                ..Default::default()
            },
        };
        assert_eq!(route(&request).unwrap().model.id, "llama-3.3-70b-versatile");
    }

    #[test]
    fn goal_plans_use_the_configured_cloud_model_when_cloud_is_allowed() {
        let request = RouteRequest {
            artifacts: Artifacts {
                text: Some("Organize my documents".into()),
                ..Default::default()
            },
            instruction: "Create a plan".into(),
            action: Some(Action::GoalPlan),
            privacy_mode: PrivacyMode::CloudAllowed,
            cost_policy: CostPolicy::Cheapest,
            model_preferences: ModelPreferences {
                cloud_model: "llama-3.3-70b-versatile".into(),
                ..Default::default()
            },
        };
        assert_eq!(route(&request).unwrap().model.id, "llama-3.3-70b-versatile");
    }

    #[test]
    fn agent_loop_payload_requires_one_structured_action() {
        let payload = agent_loop_payload("gpt-5.6-sol", "Open Canva", "url: google.com", "");
        assert_eq!(payload["text"]["format"]["strict"], true);
        assert_eq!(
            payload["text"]["format"]["schema"]["properties"]["action"]["enum"][0],
            "click"
        );
    }
    #[test]
    fn ollama_embed_parses_json_and_returns_first_embedding() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1_024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= header_end + content_length {
                    break;
                }
            }
            let body = r#"{"model":"nomic-embed-text","embeddings":[[0.1, 0.2, 0.3]]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let embedding = ollama_embed(
            &endpoint,
            "nomic-embed-text",
            "test input",
        )
        .unwrap();
        server.join().unwrap();
        
        assert_eq!(embedding, vec![0.1, 0.2, 0.3]);
    }
}
