use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tiktoken_rs::o200k_base;

use super::claude::{CreateMessageParams as ClaudeCreateMessageParams, *};
use crate::types::claude::{ImageSource, Message};

/// Convert OAI ImageUrl to Claude Image format
fn normalize_block(block: ContentBlock) -> Option<ContentBlock> {
    match block {
        ContentBlock::Text { .. } => Some(block),
        ContentBlock::Image { .. } => Some(block),
        ContentBlock::ImageUrl { image_url } => {
            ImageSource::from_image_url(&image_url.url).map(|source| ContentBlock::Image {
                source,
                cache_control: None,
            })
        }
        _ => Some(block),
    }
}

/// Normalize all blocks in a message content
/// Returns None if the message becomes empty after filtering
fn normalize_message(msg: Message) -> Option<Message> {
    let content = match msg.content {
        MessageContent::Blocks { content } => {
            let blocks: Vec<_> = content.into_iter().filter_map(normalize_block).collect();
            // skip empty messages
            if blocks.is_empty() {
                return None;
            }
            MessageContent::Blocks { content: blocks }
        }
        other => other,
    };
    Some(Message {
        role: msg.role,
        content,
    })
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    #[serde(alias = "minimal")]
    Min = 128,
    Low = 256,
    Auto = 1024,
    #[default]
    Medium = 256 * 8,
    High = 256 * 8 * 8,
    #[serde(alias = "x_high")]
    Xhigh = 256 * 8 * 8 * 2,
    #[serde(alias = "maximum")]
    Max = 256 * 8 * 8 * 4,
}

impl From<CreateMessageParams> for ClaudeCreateMessageParams {
    fn from(params: CreateMessageParams) -> Self {
        let (systems, messages): (Vec<Message>, Vec<Message>) = params
            .messages
            .into_iter()
            .partition(|m| m.role == Role::System);
        let systems = systems
            .into_iter()
            .map(|m| m.content)
            .flat_map(|c| match c {
                MessageContent::Text { content } => vec![ContentBlock::text(content)],
                MessageContent::Blocks { content } => content,
            })
            .filter(|b| matches!(b, ContentBlock::Text { .. }))
            .map(|b| json!(b))
            .collect::<Vec<_>>();
        let system = (!systems.is_empty()).then(|| json!(systems));
        // normalize messages (convert ImageUrl to Image, skip empty messages)
        let messages = messages.into_iter().filter_map(normalize_message).collect();
        Self {
            max_tokens: (params.max_tokens.or(params.max_completion_tokens))
                .unwrap_or_else(default_max_tokens),
            system,
            messages,
            model: params.model,
            container: None,
            context_management: None,
            mcp_servers: None,
            stop_sequences: params.stop,
            thinking: params
                .thinking
                .or_else(|| params.reasoning_effort.clone().map(|e| Thinking::new(e as u64))),
            temperature: params.temperature,
            stream: params.stream,
            top_k: params.top_k,
            top_p: params.top_p,
            tools: params.tools,
            tool_choice: params.tool_choice,
            metadata: params.metadata,
            output_config: params.reasoning_effort.and_then(|e| {
                // Rank-based 1:1 mapping: ST dropdown position -> Claude level
                let effort = match e {
                    Effort::Auto => return None,
                    Effort::Min => OutputEffort::Low,
                    Effort::Low => OutputEffort::Medium,
                    Effort::Medium => OutputEffort::High,
                    Effort::High | Effort::Xhigh => OutputEffort::Xhigh,
                    Effort::Max => OutputEffort::Max,
                };
                Some(OutputConfig {
                    effort: Some(effort),
                    format: None,
                })
            }),
            output_format: None,
            service_tier: None,
            n: params.n,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct CreateMessageParams {
    /// Maximum number of tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Input messages for the conversation
    pub messages: Vec<Message>,
    /// Model to use
    pub model: String,
    /// Reasoning effort for response generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Effort>,
    /// Frequency penalty for response generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    /// Temperature for response generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Custom stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// Whether to stream the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Thinking mode configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    /// Top-k sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Top-p sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Logit bias for token generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<Value>,
    /// Tools that the model may use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// How the model should use tools
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Request metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Number of completions to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
}

impl CreateMessageParams {
    pub fn count_tokens(&self) -> u32 {
        let bpe = o200k_base().expect("Failed to get encoding");
        let messages = self
            .messages
            .iter()
            .map(|msg| match msg.content {
                MessageContent::Text { ref content } => content.to_string(),
                MessageContent::Blocks { ref content } => content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text, .. } => text,
                        _ => "",
                    })
                    .collect::<String>(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        bpe.encode_with_special_tokens(&messages).len() as u32
    }
}
