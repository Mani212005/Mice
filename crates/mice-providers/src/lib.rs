//! Capability-based provider routing. Network transports are intentionally isolated
//! behind the provider clients so the router remains deterministic and testable.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_LOCAL_MODEL: &str = "gemma3:4b";
pub const DEFAULT_CLOUD_MODEL: &str = "gpt-5.6-luna";

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
    },
    ModelDescriptor {
        id: "phi4-mini",
        locality: Locality::Local,
        vision: false,
        image_gen: false,
        reasoning_tier: 1,
        speed_rank: 0,
        cost_rank: 0,
    },
    ModelDescriptor {
        id: "gpt-oss:20b",
        locality: Locality::Local,
        vision: false,
        image_gen: false,
        reasoning_tier: 1,
        speed_rank: 2,
        cost_rank: 0,
    },
    ModelDescriptor {
        id: DEFAULT_CLOUD_MODEL,
        locality: Locality::Cloud,
        vision: true,
        image_gen: false,
        reasoning_tier: 2,
        speed_rank: 0,
        cost_rank: 1,
    },
    ModelDescriptor {
        id: "gpt-5.6-terra",
        locality: Locality::Cloud,
        vision: true,
        image_gen: false,
        reasoning_tier: 3,
        speed_rank: 1,
        cost_rank: 2,
    },
    ModelDescriptor {
        id: "gpt-5.6-sol",
        locality: Locality::Cloud,
        vision: true,
        image_gen: false,
        reasoning_tier: 4,
        speed_rank: 2,
        cost_rank: 3,
    },
    ModelDescriptor {
        id: "gpt-image-2",
        locality: Locality::Cloud,
        vision: true,
        image_gen: true,
        reasoning_tier: 0,
        speed_rank: 3,
        cost_rank: 2,
    },
    ModelDescriptor {
        id: "llama-3.3-70b-versatile",
        locality: Locality::Cloud,
        vision: false,
        image_gen: false,
        reasoning_tier: 3,
        speed_rank: 0,
        cost_rank: 1,
    },
    ModelDescriptor {
        id: "llama-3.1-8b-instant",
        locality: Locality::Cloud,
        vision: false,
        image_gen: false,
        reasoning_tier: 2,
        speed_rank: 0,
        cost_rank: 1,
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
        "text":format!("You are MICE, a careful browser helper. Your speaking style is {persona}. Goal: {goal}\n\nCurrent page observation:\n{observation}\n\nRecent action history:\n{history}\n\nChoose exactly one next action. Use supplied candidate_id values exactly; never invent selectors. Narrate in plain language. Prefer handoff rather than guessing. Never fill passwords, one-time codes, or payment fields; never click login, payment, purchase, transfer, final-submit, or file-return controls.")
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
            {"role":"system", "content":format!("You are MICE, a careful browser helper. Your speaking style is {persona}. Return only one JSON object with exactly these fields: say_to_user (string), action (click|fill|open_url|scroll|done|handoff|ask_user), candidate_id (string or null), url (string or null), value (string or null), done_summary (string or null), question (string or null). Choose exactly one action. Copy candidate_id exactly from the supplied candidates; never invent selectors. Narrate plainly and prefer handoff rather than guessing. Never fill passwords, one-time codes, or payment fields; never click login, payment, purchase, transfer, final-submit, or file-return controls.")},
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
}
