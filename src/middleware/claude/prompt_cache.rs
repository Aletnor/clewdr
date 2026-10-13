//! Prompt-cache breakpoints for the Claude Code channel.
//!
//! Ported from gproxy v2.9.9 (`channel/shaping/claude_magic_cache.rs` and
//! `channel/shaping/claude_cache_control/`), reshaped onto clewdr's typed
//! request body. Three passes run in order, and every one of them respects
//! Anthropic's cap of four breakpoints per request:
//!
//! 1. `magic`: the three gproxy trigger strings. A client that embeds one in
//!    a text block gets that string stripped and a breakpoint stamped on the
//!    block (configured / 5m / 1h ttl). Same strings as gproxy, so a preset
//!    written for one proxy works against the other.
//! 2. `sanitize`: whitespace-only text blocks and thinking blocks that sit on
//!    a non-assistant turn are dropped, because upstream rejects both. A
//!    marker on a dropped block is handed to the nearest cacheable block
//!    before it, so a breakpoint never silently disappears.
//! 3. `auto`: clewdr's own addition. When the client sent no breakpoint at
//!    all (SillyTavern with caching off, most OpenAI-format clients), stamp
//!    the last system block and, by default, the last cacheable block of the
//!    last user message. Everything before a breakpoint is what gets cached,
//!    so those two spots cover the system prompt and the whole conversation
//!    prefix; on the next turn upstream looks back from the new breakpoint
//!    and finds the old one.
//!
//!    That only pays off when the conversation is a growing prefix. A client
//!    that injects at depth (author's note, lorebook, an extension that keeps
//!    a state block four messages from the end) or rewrites older messages by
//!    depth (a regex that swaps floors past N for their summaries) changes
//!    the tail every turn, and a breakpoint on the last user message then
//!    writes the whole tail into cache each time without ever reading it
//!    back. `depth` switches the auto pass to SillyTavern's cachingAtDepth
//!    rule instead: count role switches from the end, skipping any trailing
//!    assistant prefill, and stamp the messages at `depth` and `depth + 2`.
//!    Next turn the conversation is two messages longer, the old `depth`
//!    stamp sits at `depth + 2`, and the prefix up to it is found again. The
//!    last `depth` messages stay uncached at full price, which is still
//!    cheaper than writing them at the cache-write rate every turn. Pick
//!    `depth` past the deepest thing that moves or changes.
//!
//! Nothing here can fail. A body the passes do not understand is left alone,
//! and a body the client already marked up is respected as it came.

use serde_json::{Map, Value, json};
use tracing::debug;

use crate::types::claude::{
    CacheControlEphemeral, CacheControlType, ContentBlock, CreateMessageParams, Message,
    MessageContent, Role,
};

/// Anthropic accepts at most this many breakpoints in one request.
pub const MAX_BREAKPOINTS: usize = 4;

/// Beta token that unlocks the one hour ttl.
pub const EXTENDED_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";

// Frozen trigger strings, byte for byte the ones gproxy recognises.
const MAGIC_AUTO: &str =
    "GPROXY_MAGIC_STRING_TRIGGER_CACHING_CREATE_7D9ASD7A98SD7A9S8D79ASC98A7FNKJBVV80SCMSHDSIUCH";
const MAGIC_5M: &str =
    "GPROXY_MAGIC_STRING_TRIGGER_CACHING_CREATE_49VA1S5V19GR4G89W2V695G9W9GV52W95V198WV5W2FC9DF";
const MAGIC_1H: &str =
    "GPROXY_MAGIC_STRING_TRIGGER_CACHING_CREATE_1FAS5GV9R5H29T5Y2J9584K6O95M2NBVW52C95CX984FRJY";

/// What the config asked for.
#[derive(Debug, Clone, Default)]
pub struct CacheOptions {
    /// Stamp system tail + last user message when the client sent nothing.
    pub auto: bool,
    /// Honour the gproxy trigger strings.
    pub magic: bool,
    /// "5m" or "1h". Anything else means upstream's default, which is 5m.
    pub ttl: Option<String>,
    /// Some(n): auto stamps the messages n and n + 2 role switches from the
    /// end instead of the last user message. None: last user message.
    pub depth: Option<usize>,
}

/// What actually happened, for the log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheOutcome {
    /// Breakpoints in the body after all passes, client's included.
    pub breakpoints: usize,
    /// Trigger strings found and stripped, whether or not they got a marker.
    pub magic_hits: usize,
    /// Whether the auto pass placed anything.
    pub auto_stamped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ttl {
    Default,
    FiveMinutes,
    OneHour,
}

impl Ttl {
    fn from_config(ttl: Option<&str>) -> Self {
        match ttl.map(str::trim) {
            Some("1h") => Ttl::OneHour,
            Some("5m") => Ttl::FiveMinutes,
            _ => Ttl::Default,
        }
    }

    fn as_str(self) -> Option<&'static str> {
        match self {
            Ttl::Default => None,
            Ttl::FiveMinutes => Some("5m"),
            Ttl::OneHour => Some("1h"),
        }
    }
}

fn control(ttl: Ttl) -> CacheControlEphemeral {
    CacheControlEphemeral {
        type_: CacheControlType::Ephemeral,
        ttl: ttl.as_str().map(str::to_owned),
    }
}

fn control_value(ttl: Ttl) -> Value {
    json!(control(ttl))
}

/// Run the three passes over `body`. Always safe to call.
pub fn apply(body: &mut CreateMessageParams, opts: &CacheOptions) -> CacheOutcome {
    let ttl = Ttl::from_config(opts.ttl.as_deref());
    let mut outcome = CacheOutcome::default();

    let mut remaining = MAX_BREAKPOINTS.saturating_sub(count_breakpoints(body));
    if opts.magic {
        outcome.magic_hits = magic(body, &mut remaining, ttl);
    }
    sanitize(body);

    if opts.auto && count_breakpoints(body) == 0 {
        outcome.auto_stamped = auto(body, ttl, opts.depth);
    }
    outcome.breakpoints = count_breakpoints(body);
    debug!(
        "prompt cache: {} breakpoint(s), {} magic hit(s), auto={}, depth={:?}",
        outcome.breakpoints, outcome.magic_hits, outcome.auto_stamped, opts.depth
    );
    outcome
}

// ---------------------------------------------------------------- counting

/// The `cache_control` slot of a typed block, whatever its state, or None for
/// the block types that cannot carry one.
fn cache_control_of(block: &ContentBlock) -> Option<&Option<CacheControlEphemeral>> {
    match block {
        ContentBlock::Text { cache_control, .. }
        | ContentBlock::Image { cache_control, .. }
        | ContentBlock::Document { cache_control, .. }
        | ContentBlock::SearchResult { cache_control, .. }
        | ContentBlock::ToolUse { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. }
        | ContentBlock::ToolReference { cache_control, .. }
        | ContentBlock::ServerToolUse { cache_control, .. }
        | ContentBlock::WebSearchToolResult { cache_control, .. }
        | ContentBlock::WebFetchToolResult { cache_control, .. }
        | ContentBlock::CodeExecutionToolResult { cache_control, .. }
        | ContentBlock::BashCodeExecutionToolResult { cache_control, .. }
        | ContentBlock::TextEditorCodeExecutionToolResult { cache_control, .. }
        | ContentBlock::ToolSearchToolResult { cache_control, .. }
        | ContentBlock::McpToolUse { cache_control, .. }
        | ContentBlock::McpToolResult { cache_control, .. }
        | ContentBlock::ContainerUpload { cache_control, .. } => Some(cache_control),
        ContentBlock::ImageUrl { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. }
        | ContentBlock::Unknown(_) => None,
    }
}

fn has_breakpoint(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Unknown(value) => value.get("cache_control").is_some(),
        other => cache_control_of(other).is_some_and(|slot| slot.is_some()),
    }
}

fn count_breakpoints(body: &CreateMessageParams) -> usize {
    let system = match &body.system {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("cache_control").is_some())
            .count(),
        Some(Value::Object(obj)) => usize::from(obj.contains_key("cache_control")),
        _ => 0,
    };
    // Tool definitions come in more than one shape; the json is the one thing
    // they all share, and a request with tools is rare here anyway.
    let tools = body
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter(|tool| {
                    serde_json::to_value(tool)
                        .ok()
                        .is_some_and(|v| v.get("cache_control").is_some())
                })
                .count()
        })
        .unwrap_or(0);
    let messages = body
        .messages
        .iter()
        .map(|message| match &message.content {
            MessageContent::Blocks { content } => content.iter().filter(|b| has_breakpoint(b)).count(),
            MessageContent::Text { .. } => 0,
        })
        .sum::<usize>();
    system + tools + messages
}

// ------------------------------------------------------------ cacheability

/// Mutable slot of a typed block that may legally carry a marker in this
/// position. Text must be non-blank; images and documents only cache on user
/// turns; thinking never does.
fn cache_slot(block: &mut ContentBlock, role: Role) -> Option<&mut Option<CacheControlEphemeral>> {
    match block {
        ContentBlock::Text {
            text, cache_control, ..
        } => (!text.trim().is_empty()).then_some(cache_control),
        ContentBlock::Image { cache_control, .. } | ContentBlock::Document { cache_control, .. } => {
            (role == Role::User).then_some(cache_control)
        }
        ContentBlock::SearchResult { cache_control, .. }
        | ContentBlock::ToolUse { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. }
        | ContentBlock::ToolReference { cache_control, .. }
        | ContentBlock::ServerToolUse { cache_control, .. }
        | ContentBlock::WebSearchToolResult { cache_control, .. }
        | ContentBlock::WebFetchToolResult { cache_control, .. }
        | ContentBlock::CodeExecutionToolResult { cache_control, .. }
        | ContentBlock::BashCodeExecutionToolResult { cache_control, .. }
        | ContentBlock::TextEditorCodeExecutionToolResult { cache_control, .. }
        | ContentBlock::ToolSearchToolResult { cache_control, .. }
        | ContentBlock::McpToolUse { cache_control, .. }
        | ContentBlock::McpToolResult { cache_control, .. }
        | ContentBlock::ContainerUpload { cache_control, .. } => Some(cache_control),
        ContentBlock::ImageUrl { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. }
        | ContentBlock::Unknown(_) => None,
    }
}

/// Same question for a system block, which clewdr keeps as raw json.
fn is_cacheable_value(block: &Map<String, Value>) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("thinking" | "redacted_thinking") => false,
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        _ => true,
    }
}

fn is_blank_text_value(block: &Value) -> bool {
    match block {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Object(obj) => {
            obj.get("type").and_then(Value::as_str) == Some("text")
                && obj
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|text| text.trim().is_empty())
        }
        _ => false,
    }
}

// ------------------------------------------------------------------ magic

/// Strip every trigger string present; report the ttl of the first one in
/// declaration order, or None when the text carried no trigger.
fn strip_magic(text: &mut String) -> Option<Ttl> {
    let mut matched = None;
    for (id, ttl) in [
        (MAGIC_AUTO, Ttl::Default),
        (MAGIC_5M, Ttl::FiveMinutes),
        (MAGIC_1H, Ttl::OneHour),
    ] {
        if text.contains(id) {
            *text = text.replace(id, "");
            matched.get_or_insert(ttl);
        }
    }
    matched
}

/// A block that carried a trigger gets the marker even if the trigger was all
/// it held: the sanitize pass that follows drops the empty block and hands the
/// marker to the block before it, exactly as gproxy does.
fn magic(body: &mut CreateMessageParams, remaining: &mut usize, configured: Ttl) -> usize {
    let mut hits = 0;
    let pick = |found: Ttl| if found == Ttl::Default { configured } else { found };

    if let Some(Value::Array(blocks)) = body.system.as_mut() {
        for block in blocks.iter_mut() {
            let Some(obj) = block.as_object_mut() else {
                continue;
            };
            let Some(Value::String(text)) = obj.get_mut("text") else {
                continue;
            };
            let Some(found) = strip_magic(text) else {
                continue;
            };
            hits += 1;
            let is_text = obj.get("type").and_then(Value::as_str) == Some("text");
            if *remaining > 0 && is_text && !obj.contains_key("cache_control") {
                obj.insert("cache_control".to_owned(), control_value(pick(found)));
                *remaining -= 1;
            }
        }
    }

    for message in body.messages.iter_mut() {
        let promoted = match &mut message.content {
            MessageContent::Text { content } => {
                strip_magic(content).map(|found| (std::mem::take(content), found))
            }
            MessageContent::Blocks { content } => {
                for block in content.iter_mut() {
                    let ContentBlock::Text {
                        text, cache_control, ..
                    } = block
                    else {
                        continue;
                    };
                    let Some(found) = strip_magic(text) else {
                        continue;
                    };
                    hits += 1;
                    if *remaining > 0 && cache_control.is_none() {
                        *cache_control = Some(control(pick(found)));
                        *remaining -= 1;
                    }
                }
                None
            }
        };
        if let Some((text, found)) = promoted {
            hits += 1;
            let cache_control = (*remaining > 0).then(|| {
                *remaining -= 1;
                control(pick(found))
            });
            message.content = MessageContent::Blocks {
                content: vec![ContentBlock::Text {
                    text,
                    cache_control,
                    citations: None,
                }],
            };
        }
    }
    hits
}

// --------------------------------------------------------------- sanitize

fn sanitize(body: &mut CreateMessageParams) {
    if let Some(Value::Array(blocks)) = body.system.as_mut() {
        let mut kept: Vec<Value> = Vec::with_capacity(blocks.len());
        for block in std::mem::take(blocks) {
            if is_blank_text_value(&block) {
                if let Some(control) = block.get("cache_control").cloned() {
                    reattach_system(&mut kept, control);
                }
                continue;
            }
            kept.push(block);
        }
        *blocks = kept;
    }

    let mut kept_messages: Vec<Message> = Vec::with_capacity(body.messages.len());
    for mut message in std::mem::take(&mut body.messages) {
        let role = message.role;
        let taken = match &mut message.content {
            MessageContent::Blocks { content } => Some(std::mem::take(content)),
            MessageContent::Text { .. } => None,
        };
        let Some(blocks) = taken else {
            kept_messages.push(message);
            continue;
        };
        let mut kept: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
        for block in blocks {
            if role != Role::Assistant
                && matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            {
                continue;
            }
            if let ContentBlock::Text {
                text, cache_control, ..
            } = &block
                && text.trim().is_empty()
            {
                if let Some(control) = cache_control.clone()
                    && !reattach_blocks(&mut kept, role, &control)
                {
                    reattach_messages(&mut kept_messages, &control);
                }
                continue;
            }
            kept.push(block);
        }
        if kept.is_empty() {
            continue;
        }
        message.content = MessageContent::Blocks { content: kept };
        kept_messages.push(message);
    }
    body.messages = kept_messages;
}

/// Hand a marker to the last cacheable system block already kept. A block that
/// has its own marker counts as taking it, so nothing doubles up.
fn reattach_system(kept: &mut [Value], control: Value) -> bool {
    for block in kept.iter_mut().rev() {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        if !is_cacheable_value(obj) {
            continue;
        }
        if !obj.contains_key("cache_control") {
            obj.insert("cache_control".to_owned(), control);
        }
        return true;
    }
    false
}

fn reattach_blocks(kept: &mut [ContentBlock], role: Role, control: &CacheControlEphemeral) -> bool {
    for block in kept.iter_mut().rev() {
        let Some(slot) = cache_slot(block, role) else {
            continue;
        };
        if slot.is_none() {
            *slot = Some(control.clone());
        }
        return true;
    }
    false
}

fn reattach_messages(messages: &mut [Message], control: &CacheControlEphemeral) -> bool {
    for message in messages.iter_mut().rev() {
        let role = message.role;
        let MessageContent::Blocks { content } = &mut message.content else {
            continue;
        };
        if reattach_blocks(content, role, control) {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------------- auto

fn auto(body: &mut CreateMessageParams, ttl: Ttl, depth: Option<usize>) -> bool {
    let mut stamped = false;

    if let Some(Value::Array(blocks)) = body.system.as_mut()
        && let Some(obj) = blocks
            .iter_mut()
            .rev()
            .filter_map(Value::as_object_mut)
            .find(|obj| is_cacheable_value(obj))
    {
        obj.insert("cache_control".to_owned(), control_value(ttl));
        stamped = true;
    }

    match depth {
        Some(depth) => stamped |= stamp_at_depth(body, depth, ttl),
        None => {
            if let Some(message) = body.messages.iter_mut().rev().find(|m| m.role == Role::User) {
                stamped |= stamp_message(message, ttl);
            }
        }
    }
    stamped
}

/// SillyTavern's cachingAtDepth rule. Walk back from the end, skip trailing
/// assistant messages (a prefill is never worth caching), count role
/// switches, and stamp the message where the count hits `depth` and
/// `depth + 2`. Both stamps are needed: next turn the conversation has grown
/// by a user and an assistant message, the old `depth` stamp now sits at
/// `depth + 2`, and upstream finds it there when it looks back from the new
/// stamps.
fn stamp_at_depth(body: &mut CreateMessageParams, depth: usize, ttl: Ttl) -> bool {
    let mut stamped = false;
    let mut passed_prefill = false;
    let mut level = 0usize;
    let mut previous: Option<Role> = None;
    for message in body.messages.iter_mut().rev() {
        if !passed_prefill && message.role == Role::Assistant {
            continue;
        }
        passed_prefill = true;
        if previous == Some(message.role) {
            continue;
        }
        if level == depth || level == depth + 2 {
            stamped |= stamp_message(message, ttl);
        }
        if level == depth + 2 {
            break;
        }
        level += 1;
        previous = Some(message.role);
    }
    stamped
}

/// Stamp the last cacheable block of one message. Plain-text content is
/// promoted to a single block so it can carry the marker.
fn stamp_message(message: &mut Message, ttl: Ttl) -> bool {
    let role = message.role;
    let promoted = match &mut message.content {
        MessageContent::Text { content } => {
            (!content.trim().is_empty()).then(|| std::mem::take(content))
        }
        MessageContent::Blocks { content } => {
            if let Some(slot) = content
                .iter_mut()
                .rev()
                .find_map(|block| cache_slot(block, role))
            {
                *slot = Some(control(ttl));
                return true;
            }
            None
        }
    };
    let Some(text) = promoted else {
        return false;
    };
    message.content = MessageContent::Blocks {
        content: vec![ContentBlock::Text {
            text,
            cache_control: Some(control(ttl)),
            citations: None,
        }],
    };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(system: Value, messages: Vec<Message>) -> CreateMessageParams {
        CreateMessageParams {
            system: Some(system),
            messages,
            model: "claude-fable-5-1".to_owned(),
            ..Default::default()
        }
    }

    fn opts(auto: bool, magic: bool, ttl: Option<&str>) -> CacheOptions {
        CacheOptions {
            auto,
            magic,
            ttl: ttl.map(str::to_owned),
            depth: None,
        }
    }

    fn control_of_message(message: &Message) -> Option<CacheControlEphemeral> {
        let MessageContent::Blocks { content } = &message.content else {
            return None;
        };
        content
            .iter()
            .rev()
            .find_map(|block| cache_control_of(block).and_then(|slot| slot.clone()))
    }

    #[test]
    fn depth_stamps_two_levels_and_leaves_the_tail_alone() {
        // u0 a1 u2 a3 u4 a5 u6 a7 u8 a9 u10 + prefill; depth 3 -> u10 is level
        // 0, a9 level 1, u8 level 2, a7 level 3, u6 level 4, a5 level 5.
        let mut messages: Vec<Message> = (0..11)
            .map(|i| {
                let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
                Message::new_text(role, format!("m{i}"))
            })
            .collect();
        messages.push(Message::new_text(Role::Assistant, "prefill"));
        let mut b = body(json!([{"type":"text","text":"sys"}]), messages);
        let mut o = opts(true, false, Some("1h"));
        o.depth = Some(3);
        let out = apply(&mut b, &o);
        assert!(out.auto_stamped);
        assert_eq!(out.breakpoints, 3);
        let stamped: Vec<usize> = b
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| control_of_message(m).is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(stamped, vec![5, 7]);
        assert_eq!(
            control_of_message(&b.messages[7]).and_then(|c| c.ttl),
            Some("1h".to_owned())
        );
        // The last user message and the prefill were not touched.
        assert!(matches!(b.messages[10].content, MessageContent::Text { .. }));
        assert!(matches!(b.messages[11].content, MessageContent::Text { .. }));
    }

    #[test]
    fn depth_deeper_than_the_chat_stamps_only_the_system() {
        let mut b = body(
            json!([{"type":"text","text":"sys"}]),
            vec![
                Message::new_text(Role::User, "hi"),
                Message::new_text(Role::Assistant, "hello"),
                Message::new_text(Role::User, "again"),
            ],
        );
        let mut o = opts(true, false, None);
        o.depth = Some(12);
        let out = apply(&mut b, &o);
        assert!(out.auto_stamped);
        assert_eq!(out.breakpoints, 1);
        assert!(b.messages.iter().all(|m| control_of_message(m).is_none()));
    }

    fn system_blocks(body: &CreateMessageParams) -> Vec<Value> {
        body.system
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn last_user_control(body: &CreateMessageParams) -> Option<CacheControlEphemeral> {
        let message = body.messages.iter().rev().find(|m| m.role == Role::User)?;
        let MessageContent::Blocks { content } = &message.content else {
            return None;
        };
        content
            .iter()
            .rev()
            .find_map(|block| cache_control_of(block).and_then(|slot| slot.clone()))
    }

    #[test]
    fn auto_stamps_system_tail_and_last_user_message() {
        let mut b = body(
            json!([{"type":"text","text":"billing"},{"type":"text","text":"persona"}]),
            vec![
                Message::new_text(Role::User, "hi"),
                Message::new_text(Role::Assistant, "hello"),
                Message::new_text(Role::User, "again"),
            ],
        );
        let out = apply(&mut b, &opts(true, true, None));
        assert_eq!(out.breakpoints, 2);
        assert!(out.auto_stamped);
        let system = system_blocks(&b);
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");
        assert!(system[1]["cache_control"].get("ttl").is_none());
        assert!(matches!(b.messages[0].content, MessageContent::Text { .. }));
        assert_eq!(last_user_control(&b).map(|c| c.ttl), Some(None));
    }

    #[test]
    fn auto_stays_out_when_client_brought_its_own() {
        let mut b = body(
            json!([{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}]),
            vec![Message::new_text(Role::User, "hi")],
        );
        let out = apply(&mut b, &opts(true, true, None));
        assert_eq!(out.breakpoints, 1);
        assert!(!out.auto_stamped);
        assert!(matches!(b.messages[0].content, MessageContent::Text { .. }));
    }

    #[test]
    fn ttl_from_config_lands_on_auto_markers() {
        let mut b = body(
            json!([{"type":"text","text":"sys"}]),
            vec![Message::new_text(Role::User, "hi")],
        );
        apply(&mut b, &opts(true, false, Some("1h")));
        assert_eq!(system_blocks(&b)[0]["cache_control"]["ttl"], "1h");
        assert_eq!(
            last_user_control(&b).and_then(|c| c.ttl),
            Some("1h".to_owned())
        );
    }

    #[test]
    fn magic_string_is_stripped_and_becomes_a_marker() {
        let mut b = body(
            json!([{"type":"text","text":format!("lore {MAGIC_5M} more")}]),
            vec![Message::new_blocks(
                Role::User,
                vec![ContentBlock::text(format!("q {MAGIC_1H}"))],
            )],
        );
        let out = apply(&mut b, &opts(false, true, None));
        assert_eq!(out.magic_hits, 2);
        assert_eq!(out.breakpoints, 2);
        let system = system_blocks(&b);
        assert_eq!(system[0]["text"], "lore  more");
        assert_eq!(system[0]["cache_control"]["ttl"], "5m");
        let MessageContent::Blocks { content } = &b.messages[0].content else {
            panic!("blocks expected");
        };
        let ContentBlock::Text {
            text, cache_control, ..
        } = &content[0]
        else {
            panic!("text expected");
        };
        assert_eq!(text, "q ");
        assert_eq!(
            cache_control.as_ref().and_then(|c| c.ttl.clone()),
            Some("1h".to_owned())
        );
    }

    #[test]
    fn magic_only_block_hands_its_marker_backward() {
        let mut b = body(
            json!([{"type":"text","text":"keep"},{"type":"text","text":MAGIC_AUTO}]),
            vec![Message::new_text(Role::User, "hi")],
        );
        let out = apply(&mut b, &opts(false, true, None));
        let system = system_blocks(&b);
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], "keep");
        assert!(system[0].get("cache_control").is_some());
        assert_eq!(out.breakpoints, 1);
    }

    #[test]
    fn breakpoints_never_exceed_four() {
        let blocks: Vec<Value> = (0..6)
            .map(|i| json!({"type":"text","text":format!("b{i} {MAGIC_AUTO}")}))
            .collect();
        let mut b = body(Value::Array(blocks), vec![Message::new_text(Role::User, "hi")]);
        let out = apply(&mut b, &opts(true, true, None));
        assert_eq!(out.breakpoints, MAX_BREAKPOINTS);
        assert_eq!(out.magic_hits, 6);
        assert!(!out.auto_stamped);
    }

    #[test]
    fn thinking_on_a_user_turn_is_dropped_and_assistant_kept() {
        let thinking = || ContentBlock::Thinking {
            signature: "s".to_owned(),
            thinking: "t".to_owned(),
        };
        let mut b = body(
            json!([{"type":"text","text":"sys"}]),
            vec![
                Message::new_blocks(Role::Assistant, vec![thinking(), ContentBlock::text("a")]),
                Message::new_blocks(
                    Role::User,
                    vec![thinking(), ContentBlock::text("   "), ContentBlock::text("u")],
                ),
            ],
        );
        apply(&mut b, &opts(false, false, None));
        let MessageContent::Blocks { content } = &b.messages[0].content else {
            panic!("blocks expected");
        };
        assert_eq!(content.len(), 2);
        let MessageContent::Blocks { content } = &b.messages[1].content else {
            panic!("blocks expected");
        };
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn nothing_to_do_when_everything_is_off() {
        let mut b = body(
            json!([{"type":"text","text":"sys"}]),
            vec![Message::new_text(Role::User, "hi")],
        );
        let before = json!(b);
        let out = apply(&mut b, &opts(false, false, None));
        assert_eq!(out, CacheOutcome::default());
        assert_eq!(json!(b), before);
    }
}
