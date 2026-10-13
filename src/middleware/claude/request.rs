use std::{
    env,
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    sync::LazyLock,
    vec,
};

use axum::{
    Json,
    extract::{FromRequest, Request},
};
use http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    config::{CLAUDE_CODE_BILLING_SALT, CLAUDE_CODE_VERSION, CLEWDR_CONFIG},
    error::ClewdrError,
    middleware::claude::{ClaudeApiFormat, ClaudeContext},
    types::{
        claude::{
            ContentBlock, CreateMessageParams, Message, MessageContent, OutputConfig,
            OutputEffort, Role, Thinking, Usage,
        },
        oai::CreateMessageParams as OaiCreateMessageParams,
    },
};

/// A custom extractor that unifies different API formats
///
/// This extractor processes incoming requests, handling differences between
/// Claude and OpenAI API formats, and applies preprocessing to ensure consistent
/// handling throughout the application. It also detects and handles test messages
/// from client applications.
///
/// # Functionality
///
/// - Extracts and normalizes message parameters from different API formats
/// - Detects and processes "thinking mode" requests by modifying model names
/// - Identifies test messages and handles them appropriately
/// - Attempts to retrieve responses from cache before processing requests
/// - Provides format information via the FormatInfo extension
pub struct ClaudeWebPreprocess(pub CreateMessageParams, pub ClaudeContext);

/// Contains information about the API format and streaming status
///
/// This structure is passed through the request pipeline to inform
/// handlers and response processors about the API format being used
/// and whether the response should be streamed.
#[derive(Debug, Clone)]
pub struct ClaudeWebContext {
    /// Whether the response should be streamed
    pub(super) stream: bool,
    /// The API format being used (Claude or OpenAI)
    pub(super) api_format: ClaudeApiFormat,
    /// The stop sequence used for the request
    pub(super) stop_sequences: Vec<String>,
    /// User information about input and output tokens
    pub(super) usage: Usage,
}

/// Predefined test message in Claude format for connection testing
///
/// This is a standard test message sent by clients like SillyTavern
/// to verify connectivity. The system detects these messages and
/// responds with a predefined test response to confirm service availability.
static TEST_MESSAGE_CLAUDE: LazyLock<Message> =
    LazyLock::new(|| Message::new_blocks(Role::User, vec![ContentBlock::text("Hi")]));

/// Predefined test message in OpenAI format for connection testing
static TEST_MESSAGE_OAI: LazyLock<Message> = LazyLock::new(|| Message::new_text(Role::User, "Hi"));

struct NormalizeRequest(CreateMessageParams, ClaudeApiFormat);

const CLAUDE_CODE_ENTRYPOINT_ENV: &str = "CLAUDE_CODE_ENTRYPOINT";

fn prepend_system_blocks(body: &mut CreateMessageParams, blocks: Vec<ContentBlock>) {
    if blocks.is_empty() {
        return;
    }

    let mut prefixed = blocks
        .into_iter()
        .map(|block| json!(block))
        .collect::<Vec<_>>();
    match body.system.take() {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            prefixed.push(json!(ContentBlock::text(text)));
        }
        Some(Value::Array(mut systems)) => {
            prefixed.append(&mut systems);
        }
        Some(Value::Null) | None => {}
        Some(other) => prefixed.push(other),
    }
    body.system = Some(Value::Array(prefixed));
}

fn first_user_message_text(messages: &[Message]) -> &str {
    messages
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(|message| match &message.content {
            MessageContent::Text { content } => Some(content.as_str()),
            MessageContent::Blocks { content } => content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }),
        })
        .unwrap_or_default()
}

fn sample_js_code_unit(text: &str, idx: usize) -> String {
    text.encode_utf16()
        .nth(idx)
        .map(|unit| String::from_utf16_lossy(&[unit]))
        .unwrap_or_else(|| "0".to_string())
}

fn claude_code_billing_header(messages: &[Message]) -> String {
    let sampled = [4, 7, 20]
        .into_iter()
        .map(|idx| sample_js_code_unit(first_user_message_text(messages), idx))
        .collect::<String>();
    let version_hash = hex::encode(Sha256::digest(format!(
        "{CLAUDE_CODE_BILLING_SALT}{sampled}{CLAUDE_CODE_VERSION}"
    )));
    let entrypoint = env::var(CLAUDE_CODE_ENTRYPOINT_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cli".to_string());

    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION}.{}; cc_entrypoint={entrypoint}; cch=00000;",
        &version_hash[..3]
    )
}

fn drop_empty_system(body: &mut CreateMessageParams) {
    let Some(system) = body.system.take() else {
        return;
    };

    let is_empty = match &system {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(systems) => systems.is_empty()
            || systems.iter().all(|entry| match entry {
                Value::Null => true,
                Value::String(text) => text.trim().is_empty(),
                Value::Object(obj) if matches!(obj.get("type"), Some(Value::String(t)) if t == "text") => {
                    obj.get("text")
                        .and_then(Value::as_str)
                        .is_none_or(|text| text.trim().is_empty())
                }
                _ => false,
            }),
        _ => false,
    };

    body.system = (!is_empty).then_some(system);
}

fn strip_ephemeral_scope_from_system(system: &mut Value) {
    let Some(items) = system.as_array_mut() else {
        return;
    };

    for item in items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let Some(cache_control) = obj.get_mut("cache_control") else {
            continue;
        };
        let Some(cache_obj) = cache_control.as_object_mut() else {
            continue;
        };

        if let Some(ephemeral) = cache_obj.get_mut("ephemeral")
            && let Some(ephemeral_obj) = ephemeral.as_object_mut()
        {
            ephemeral_obj.remove("scope");
        }

        if matches!(cache_obj.get("type"), Some(Value::String(t)) if t == "ephemeral") {
            cache_obj.remove("scope");
        }
    }
}

fn extract_anthropic_beta_header(headers: &HeaderMap) -> Option<String> {
    let mut parts = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        if let Ok(raw) = value.to_str() {
            for token in raw.split(',') {
                let token = token.trim();
                if !token.is_empty() {
                    parts.push(token.to_string());
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

fn sanitize_messages(msgs: Vec<Message>) -> Vec<Message> {
    msgs.into_iter()
        .filter_map(|m| {
            let role = m.role;
            let content = match m.content {
                MessageContent::Text { content } => {
                    let trimmed = content.trim().to_string();
                    if role == Role::Assistant && trimmed.is_empty() {
                        return None;
                    }
                    MessageContent::Text { content: trimmed }
                }
                MessageContent::Blocks { content } => {
                    let mut new_blocks: Vec<ContentBlock> = content
                        .into_iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => {
                                let t = text.trim().to_string();
                                if t.is_empty() {
                                    None
                                } else {
                                    Some(ContentBlock::text(t))
                                }
                            }
                            other => Some(other),
                        })
                        .collect();
                    if role == Role::Assistant && new_blocks.is_empty() {
                        return None;
                    }
                    MessageContent::Blocks {
                        content: mem::take(&mut new_blocks),
                    }
                }
            };
            Some(Message { role, content })
        })
        .collect()
}

impl<S> FromRequest<S> for NormalizeRequest
where
    S: Send + Sync,
{
    type Rejection = ClewdrError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let uri = req.uri().to_string();
        let format = if uri.contains("chat/completions") {
            ClaudeApiFormat::OpenAI
        } else {
            ClaudeApiFormat::Claude
        };
        let Json(mut body) = match format {
            ClaudeApiFormat::OpenAI => {
                let Json(json) = Json::<OaiCreateMessageParams>::from_request(req, &()).await?;
                Json(json.into())
            }
            ClaudeApiFormat::Claude => Json::<CreateMessageParams>::from_request(req, &()).await?,
        };
        if CLEWDR_CONFIG.load().sanitize_messages {
            // Trim whitespace and drop empty assistant turns when enabled.
            body.messages = sanitize_messages(body.messages);
        }
        if body.model.ends_with("-thinking") {
            body.model = body.model.trim_end_matches("-thinking").to_string();
            body.thinking.get_or_insert(Thinking::new(4096));
        }
        drop_empty_system(&mut body);
        Ok(Self(body, format))
    }
}

impl<S> FromRequest<S> for ClaudeWebPreprocess
where
    S: Send + Sync,
{
    type Rejection = ClewdrError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let NormalizeRequest(body, format) = NormalizeRequest::from_request(req, &()).await?;

        // Check for test messages and respond appropriately
        if !body.stream.unwrap_or_default()
            && (body.messages == vec![TEST_MESSAGE_CLAUDE.to_owned()]
                || body.messages == vec![TEST_MESSAGE_OAI.to_owned()])
        {
            // Respond with a test message
            return Err(ClewdrError::TestMessage);
        }

        // Determine streaming status and API format
        let stream = body.stream.unwrap_or_default();

        let input_tokens = body.count_tokens();
        let info = ClaudeWebContext {
            stream,
            api_format: format,
            stop_sequences: body.stop_sequences.to_owned().unwrap_or_default(),
            usage: Usage {
                input_tokens,
                output_tokens: 0, // Placeholder for output token count
            },
        };

        Ok(Self(body, ClaudeContext::Web(info)))
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeCodeContext {
    /// Whether the response should be streamed
    pub(super) stream: bool,
    /// The API format being used (Claude or OpenAI)
    pub(super) api_format: ClaudeApiFormat,
    /// The hash of the system messages for caching purposes
    pub(super) system_prompt_hash: Option<u64>,
    /// Optional anthropic-beta header forwarded from client request
    pub(super) anthropic_beta: Option<String>,
    // Usage information for the request
    pub(super) usage: Usage,
}

pub struct ClaudeCodePreprocess(pub CreateMessageParams, pub ClaudeContext);

impl<S> FromRequest<S> for ClaudeCodePreprocess
where
    S: Send + Sync,
{
    type Rejection = ClewdrError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let mut anthropic_beta = extract_anthropic_beta_header(req.headers());
        let NormalizeRequest(mut body, format) = NormalizeRequest::from_request(req, &()).await?;
        // Handle thinking mode by modifying the model name
        if body.temperature.is_some() {
            body.top_p = None; // temperature and top_p cannot be used together in Opus-4.x
        }

        // New-generation models (Fable 5, Opus 4.8, ...) encrypt their thinking
        // and only emit visible thinking_delta events when the request asks for
        // adaptive+summarized thinking. Clients like SillyTavern don't send a
        // thinking field, so inject it here for these models. Adaptive mode also
        // requires temperature == 1 and forbids top_k/top_p, so normalize those.
        // Effort suffix on model name (e.g. claude-opus-5-high): strip it and
        // map to output_config.effort. Explicit output_config wins over suffix.
        for (suf, eff) in [
            ("-xhigh", OutputEffort::Xhigh),
            ("-high", OutputEffort::High),
            ("-medium", OutputEffort::Medium),
            ("-low", OutputEffort::Low),
            ("-max", OutputEffort::Max),
        ] {
            if let Some(stripped) = body.model.strip_suffix(suf) {
                body.model = stripped.to_string();
                let oc = body.output_config.get_or_insert(OutputConfig {
                    effort: None,
                    format: None,
                });
                if oc.effort.is_none() {
                    oc.effort = Some(eff);
                }
                break;
            }
        }
        // Re-strip -thinking in case it preceded the effort suffix
        if body.model.ends_with("-thinking") {
            body.model = body.model.trim_end_matches("-thinking").to_string();
        }

        let model_lc = body.model.to_ascii_lowercase();
        let needs_adaptive_thinking =
            model_lc.contains("fable") || model_lc.contains("opus-4-8") || model_lc.contains("opus-5") || model_lc.contains("sonnet-5");
        if needs_adaptive_thinking && !matches!(body.thinking, Some(Thinking::Disabled)) {
            body.thinking = Some(Thinking::Adaptive {
                display: Some("summarized".to_string()),
            });
            body.top_k = None;
            body.top_p = None;
            body.temperature = Some(1.0);
            if body.max_tokens < 16000 {
                body.max_tokens = 16000;
            }
        }
        if !needs_adaptive_thinking {
            if let Some(oc) = &mut body.output_config {
                oc.effort = None;
            }
            if body
                .output_config
                .as_ref()
                .is_some_and(|oc| oc.effort.is_none() && oc.format.is_none())
            {
                body.output_config = None;
            }
        }

        // Check for test messages and respond appropriately
        if !body.stream.unwrap_or_default()
            && (body.messages == vec![TEST_MESSAGE_CLAUDE.to_owned()]
                || body.messages == vec![TEST_MESSAGE_OAI.to_owned()])
        {
            // Respond with a test message
            return Err(ClewdrError::TestMessage);
        }

        // Determine streaming status and API format
        let stream = body.stream.unwrap_or_default();

        let mut system_prefixes = vec![ContentBlock::text(claude_code_billing_header(
            &body.messages,
        ))];
        if let Some(custom_system) = CLEWDR_CONFIG
            .load()
            .custom_system
            .clone()
            .filter(|s| !s.trim().is_empty())
        {
            system_prefixes.push(ContentBlock::text(custom_system));
        }
        prepend_system_blocks(&mut body, system_prefixes);

        if let Some(system) = body.system.as_mut() {
            strip_ephemeral_scope_from_system(system);
        }

        // Prompt-cache breakpoints, see prompt_cache.rs. Runs after the
        // system prefix is final so the last system block is the whole prefix.
        let cache_opts = {
            let cfg = CLEWDR_CONFIG.load();
            super::prompt_cache::CacheOptions {
                auto: cfg.auto_cache,
                magic: cfg.magic_cache,
                ttl: cfg.cache_ttl.clone(),
                depth: usize::try_from(cfg.cache_at_depth).ok(),
            }
        };
        let cache = super::prompt_cache::apply(&mut body, &cache_opts);
        if cache.breakpoints > 0 && cache_opts.ttl.as_deref().map(str::trim) == Some("1h") {
            let beta = super::prompt_cache::EXTENDED_TTL_BETA;
            anthropic_beta = Some(match anthropic_beta.take() {
                Some(existing) if existing.split(',').any(|t| t.trim() == beta) => existing,
                Some(existing) => format!("{existing},{beta}"),
                None => beta.to_owned(),
            });
        }

        let cache_systems = body
            .system
            .as_ref()
            .and_then(Value::as_array)
            .map(|systems| {
                systems
                    .iter()
                    .filter(|s| s["cache_control"].as_object().is_some())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let system_prompt_hash = (!cache_systems.is_empty()).then(|| {
            let mut hasher = DefaultHasher::new();
            cache_systems.hash(&mut hasher);
            hasher.finish()
        });

        let input_tokens = body.count_tokens();

        let info = ClaudeCodeContext {
            stream,
            api_format: format,
            system_prompt_hash,
            anthropic_beta,
            usage: Usage {
                input_tokens,
                output_tokens: 0, // Placeholder for output token count
            },
        };

        Ok(Self(body, ClaudeContext::Code(info)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_billing_header_matches_21280_rule() {
        let messages = vec![Message::new_text(Role::User, "hey")];

        assert_eq!(
            claude_code_billing_header(&messages),
            "x-anthropic-billing-header: cc_version=2.1.280.d7b; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[test]
    fn claude_code_billing_header_uses_first_text_block_of_first_user_message() {
        let messages = vec![
            Message::new_blocks(
                Role::User,
                vec![
                    ContentBlock::Image {
                        source: crate::types::claude::ImageSource::Url {
                            url: "https://example.com/a.png".to_string(),
                        },
                        cache_control: None,
                    },
                    ContentBlock::text("abcdefg"),
                    ContentBlock::text("ignored"),
                ],
            ),
            Message::new_text(Role::User, "later"),
        ];

        assert_eq!(
            claude_code_billing_header(&messages),
            "x-anthropic-billing-header: cc_version=2.1.280.5b1; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[test]
    fn prepend_system_blocks_keeps_billing_before_custom_system() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_text(Role::User, "hey")],
            model: "claude-sonnet-4-5".to_string(),
            system: Some(json!("original system")),
            ..Default::default()
        };

        prepend_system_blocks(
            &mut body,
            vec![
                ContentBlock::text("billing"),
                ContentBlock::text("custom system"),
            ],
        );

        let systems = body.system.unwrap().as_array().cloned().unwrap();
        let texts = systems
            .iter()
            .map(|value| value["text"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["billing", "custom system", "original system"]);
    }
}
