//! Response inspection for the /code path.
//!
//! Everything here is read-only. Five things get watched:
//!   * the model id Anthropic embeds in thinking signatures, cross-checked
//!     against the model the response openly claims to be
//!   * the stop reason, so a reply that got cut short says why in the log
//!   * whether the reply actually reads as finished, because upstream will
//!     happily stamp end_turn on a sentence it abandoned halfway
//!   * how much of the output was the model talking to itself, since upstream
//!     bills the thinking chain and the reply as one indivisible number and a
//!     short answer after a long think otherwise looks like a long answer
//!   * one line of json per response, appended to a file a status page reads,
//!     so the whole picture is visible without anyone touching journalctl
//!
//! The json file holds bookkeeping only. No prompt, no reply text, ever. The
//! finished-or-not check keeps a single character in memory while the response
//! is alive and writes out only the yes/no it produced. The thinking split
//! counts each chunk as it goes past and adds the number to a running total,
//! so the words are gone by the time the function returns.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use tiktoken_rs::o200k_base_singleton;
use tracing::{debug, warn};

use crate::types::claude::StopReason;

/// Where the status page looks. Rotated in place once it gets fat, because a
/// blind sentinel is one thing and a sentinel that fills the disk is another.
///
/// The original spot under /var/log stays in use wherever the file is already
/// there. A fresh install, or a system with no such place to write, keeps the
/// file beside its config instead, so the record works without being root.
pub fn record_path() -> &'static Path {
    static PATH: LazyLock<PathBuf> = LazyLock::new(|| {
        let legacy = Path::new("/var/log/clewdr-inspect.jsonl");
        if legacy.exists() {
            return legacy.to_path_buf();
        }
        crate::config::CONFIG_PATH
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join("clewdr-inspect.jsonl")
    });
    &PATH
}
const RECORD_MAX_BYTES: u64 = 2 * 1024 * 1024;
const DETAIL_MAX_CHARS: usize = 1200;

/// The model id sits around byte 80 of the decoded blob, so a short prefix of
/// the signature is enough and the encrypted remainder never has to be touched.
const PREFIX_CHARS: usize = 256;
const MIN_CHARS: usize = 120;
const MAX_NAME_LEN: usize = 64;

/// The blob is protobuf. The model name is a length-delimited field, so the
/// byte in front of it says how long it is. Reading that length is what keeps
/// the next field's tag byte (0x38, which prints as '8') out of the name.
fn scan_model_id(raw: &[u8]) -> Option<String> {
    let needle = b"claude-";
    if raw.len() < needle.len() {
        return None;
    }
    let Some(start) = raw.windows(needle.len()).position(|w| w == needle) else {
        return None;
    };
    if start == 0 {
        return None;
    }
    let len = raw[start - 1] as usize;
    if len < needle.len() || len > MAX_NAME_LEN {
        return None;
    }
    let Some(name) = raw.get(start..start + len) else {
        return None;
    };
    let clean = name
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'.');
    if !clean {
        return None;
    }
    match std::str::from_utf8(name) {
        Ok(s) => Some(s.to_owned()),
        Err(_) => None,
    }
}

pub fn model_id_in_signature(signature: &str) -> Option<String> {
    let take = signature.len().min(PREFIX_CHARS);
    let take = take - take % 4;
    if take < MIN_CHARS {
        return None;
    }
    let Some(prefix) = signature.get(..take) else {
        return None;
    };
    let Ok(raw) = BASE64_STANDARD.decode(prefix) else {
        return None;
    };
    scan_model_id(&raw)
}

/// Silent when the two names agree, and silent when the signature cannot be
/// read at all. Only a real disagreement is worth shouting about, so an
/// unfamiliar signature layout stays quiet instead of crying wolf.
pub fn report(claimed: &str, signature: &str) {
    let Some(actual) = model_id_in_signature(signature) else {
        return;
    };
    if actual == claimed {
        debug!("signature model check ok: {}", claimed);
        return;
    }
    // One side carrying a dated snapshot id is still the same model.
    if actual.starts_with(claimed) || claimed.starts_with(actual.as_str()) {
        debug!(
            "signature model check ok: response says {}, signature says {}",
            claimed, actual
        );
        return;
    }
    warn!(
        "MODEL SIGNATURE MISMATCH: response claims {}, thinking signature says {}",
        claimed, actual
    );
}

/// Four endings mean the model said its piece. Everything else cut it short.
fn is_abnormal(reason: &str) -> bool {
    !matches!(
        reason,
        "end_turn" | "stop_sequence" | "tool_use" | "pause_turn"
    )
}

/// Upstream names the ending and says nothing else about it, and clewdr's typed
/// structs quietly drop any field they were not built to hold. So on a bad
/// ending the raw event goes to the log word for word, on the chance that
/// upstream is already telling us more than the struct can carry.
///
/// Safe to print: a message_delta holds bookkeeping only, never reply text.
pub fn report_ending_detail(reason: &str, raw: &str) {
    if !is_abnormal(reason) {
        return;
    }
    warn!("TRUNCATED detail, raw event: {}", raw.trim());
}

/// An error arriving mid-stream is a truncation that never reaches the
/// stop_reason machinery at all.
pub fn report_error_event(kind: &str, message: &str) {
    warn!(
        "TRUNCATED: upstream sent an error mid-reply: {} / {}",
        kind, message
    );
}

/// Normal endings stay at debug. Anything that cut the reply short is a warn,
/// so a truncation always leaves a named reason in the log.
pub fn report_stop_reason_str(reason: &str) {
    match reason {
        "end_turn" | "stop_sequence" | "tool_use" | "pause_turn" => {
            debug!("stop reason: {}", reason);
        }
        "max_tokens" => {
            warn!("TRUNCATED: max_tokens, the reply ran out of output budget");
        }
        "refusal" => {
            warn!("TRUNCATED: refusal, upstream cut the reply on content grounds");
        }
        "model_context_window_exceeded" => {
            warn!("TRUNCATED: context window exceeded");
        }
        other => {
            warn!("TRUNCATED: unrecognised stop reason from upstream: {}", other);
        }
    }
}

/// Biased hard towards silence. A reply may legitimately end on a quote mark,
/// a bracket, an asterisk, an emoji or any punctuation this list has never
/// heard of, so only endings that can never be an ending count as cut off: a
/// letter, a han character, or a joining punctuation mark that promises more.
fn looks_cut_off(c: char) -> bool {
    matches!(
        c,
        '，' | ',' | '、' | '；' | ';' | '：' | ':' | '(' | '（' | '【' | '「' | '『'
    ) || c.is_ascii_alphabetic()
        || ('\u{4e00}'..='\u{9fff}').contains(&c)
}

fn stop_reason_name(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::PauseTurn => "pause_turn",
        StopReason::Refusal => "refusal",
        StopReason::ModelContextWindowExceeded => "model_context_window_exceeded",
    }
}

#[derive(Default)]
struct RecordFields {
    claimed: Option<String>,
    real: Option<String>,
    /// Whether the request went upstream carrying an atis slot (see
    /// `atis.rs`). None when it never got as far as being sent.
    atis: Option<bool>,
    stop: Option<String>,
    detail: Option<String>,
    error: Option<String>,
    /// The HTTP status the upstream exchange actually ended on. A status
    /// page that can say 429 is worth more than one that says something
    /// friendly, so the code travels with the row instead of being guessed
    /// back out of the error text later.
    code: Option<u16>,
    /// Last non-whitespace character of the reply, kept only until drop and
    /// never written anywhere. One character in, one boolean out.
    tail: Option<char>,
    /// Whether the closing event made it out of the proxy. A reply upstream
    /// finished but the client never got is a different failure from a reply
    /// upstream cut short, and they should not share a row.
    sent: bool,
    /// What upstream said about the prompt cache in its usage block:
    /// tokens read back from cache, tokens written into it, and the input
    /// count billed at full price after both. None when upstream never said.
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    upstream_in: Option<u64>,
}

/// One of these lives exactly as long as one response does. Whatever the
/// response gets around to admitting is noted here, and the drop impl writes
/// the whole picture out as a single line of json.
///
/// A reply that never says how it ended says nothing at all, which is the
/// exact case that used to leave no trace. Drop runs either way, so silence
/// gets a line too.
pub struct ResponseRecord {
    started: Instant,
    stream: bool,
    format: String,
    input_tokens: u64,
    output_tokens: Arc<AtomicU64>,
    /// Running tokenizer counts for the thinking chain and for everything the
    /// client actually receives, kept apart so upstream's one output total can
    /// be divided between them at the end. Counts only, never the text.
    think_units: AtomicU64,
    reply_units: AtomicU64,
    timed: bool,
    fields: Mutex<RecordFields>,
    /// A streamed reply is still being written while this lives, well after
    /// the handler that started it has returned.
    _busy: crate::services::busy::Busy,
}

/// Measures a chunk and forgets it. The tokenizer is not the one upstream
/// bills with, so a number out of here is never reported on its own; it exists
/// to weigh the thinking chain against the reply, and a ratio survives the
/// difference between two tokenizers far better than a total would.
fn count_units(text: &str) -> u64 {
    o200k_base_singleton().encode_ordinary(text).len() as u64
}

/// Upstream sends one output_tokens and it covers the thinking chain and the
/// reply together. That total is the number worth trusting, so it is the one
/// that gets divided, in the proportion the two halves measured on the way
/// past. The halves therefore always add straight back up to it.
///
/// Nothing is reported when the total is zero or when nothing was measured,
/// because a made-up zero reads like a fact and a blank reads like a blank.
fn split_output(out: u64, think: u64, reply: u64) -> (Option<u64>, Option<u64>) {
    let measured = think.saturating_add(reply);
    if out == 0 || measured == 0 {
        return (None, None);
    }
    let think_share = ((out as u128 * think as u128) / measured as u128) as u64;
    (Some(think_share), Some(out.saturating_sub(think_share)))
}

impl ResponseRecord {
    /// The streaming case shares the running output counter the proxy already
    /// keeps, so the token totals cost nothing extra.
    pub fn for_stream(format: &str, input_tokens: u64, output_tokens: Arc<AtomicU64>) -> Self {
        Self {
            started: Instant::now(),
            stream: true,
            format: format.to_owned(),
            input_tokens,
            output_tokens,
            think_units: AtomicU64::new(0),
            reply_units: AtomicU64::new(0),
            timed: true,
            fields: Mutex::new(RecordFields::default()),
            _busy: Default::default(),
        }
    }

    /// The non-streaming case arrives whole and late, with no clock to read.
    pub fn for_single(format: &str, input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            started: Instant::now(),
            stream: false,
            format: format.to_owned(),
            input_tokens,
            output_tokens: Arc::new(AtomicU64::new(output_tokens)),
            think_units: AtomicU64::new(0),
            reply_units: AtomicU64::new(0),
            timed: false,
            // A whole body either arrived or it did not, so there is no
            // half-delivered case to watch for here.
            fields: Mutex::new(RecordFields {
                sent: true,
                ..Default::default()
            }),
            _busy: Default::default(),
        }
    }

    pub fn note_atis(&self, carried: bool) {
        if let Ok(mut f) = self.fields.lock() {
            f.atis = Some(carried);
        }
    }

    pub fn note_claimed_model(&self, claimed: &str) {
        if let Ok(mut f) = self.fields.lock() {
            f.claimed = Some(claimed.to_owned());
        }
    }

    /// Runs the signature cross-check and keeps both names for the page.
    pub fn note_signature(&self, claimed: &str, signature: &str) {
        report(claimed, signature);
        if let Ok(mut f) = self.fields.lock() {
            f.claimed = Some(claimed.to_owned());
            f.real = model_id_in_signature(signature);
        }
    }

    pub fn note_reason(&self, reason: &StopReason, raw: &str) {
        self.note_reason_str(stop_reason_name(reason), raw);
    }

    /// Also takes a stop reason clewdr's own enum does not know yet.
    pub fn note_reason_str(&self, reason: &str, raw: &str) {
        report_stop_reason_str(reason);
        report_ending_detail(reason, raw);
        if let Ok(mut f) = self.fields.lock() {
            f.stop = Some(reason.to_owned());
            if is_abnormal(reason) {
                f.detail = Some(clip(raw));
            }
        }
    }

    /// Only a size and a tail survive the call. Every earlier character is
    /// dropped on the floor before this function returns.
    pub fn note_text(&self, text: &str) {
        self.reply_units
            .fetch_add(count_units(text), Ordering::Relaxed);
        let Some(last) = text.chars().rev().find(|c| !c.is_whitespace()) else {
            return;
        };
        if let Ok(mut f) = self.fields.lock() {
            f.tail = Some(last);
        }
    }

    /// The thinking chain is output as well, and upstream charges for it inside
    /// the same number as the reply, so it has to be weighed on its own here or
    /// it stays invisible. Measured and discarded; nothing is stored.
    pub fn note_think(&self, thinking: &str) {
        self.think_units
            .fetch_add(count_units(thinking), Ordering::Relaxed);
    }

    /// Tool arguments are written by the model and reach the client, so they
    /// weigh on the reply side. They stay out of the finished-or-not check,
    /// which is about prose and would only be confused by json.
    pub fn note_tool_input(&self, partial_json: &str) {
        self.reply_units
            .fetch_add(count_units(partial_json), Ordering::Relaxed);
    }

    /// Called when the closing event has been handed to the client.
    pub fn note_sent(&self) {
        if let Ok(mut f) = self.fields.lock() {
            f.sent = true;
        }
    }

    pub fn note_error_event(&self, kind: &str, message: &str) {
        report_error_event(kind, message);
        if let Ok(mut f) = self.fields.lock() {
            f.error = Some(clip(&format!("{}: {}", kind, message)));
        }
    }

    pub fn note_transport_error(&self, detail: &str) {
        warn!("TRUNCATED: the upstream stream broke mid-reply: {}", detail);
        if let Ok(mut f) = self.fields.lock() {
            f.error = Some(clip(detail));
        }
    }

    pub fn note_status(&self, code: u16) {
        if let Ok(mut f) = self.fields.lock() {
            f.code = Some(code);
        }
    }

    /// The usage block of a message_start event or of a whole message.
    /// Only the cache numbers are kept here; the totals live elsewhere.
    pub fn note_cache_usage(&self, usage: &serde_json::Value) {
        let read = |name: &str| usage.get(name).and_then(|v| v.as_u64());
        if let Ok(mut f) = self.fields.lock() {
            f.cache_read = read("cache_read_input_tokens");
            f.cache_write = read("cache_creation_input_tokens");
            f.upstream_in = read("input_tokens");
        }
    }
}

/// A request that never became a response used to leave the status page
/// completely blank, which reads exactly like a quiet day. It gets a row now.
pub fn record_failure(format: &str, model: &str, detail: &str, code: Option<u16>) {
    let record = ResponseRecord::for_single(format, 0, 0);
    if let Ok(mut f) = record.fields.lock() {
        if !model.is_empty() {
            f.claimed = Some(model.to_owned());
        }
        f.error = Some(clip(detail));
        f.code = code;
    }
    // Drop writes the line.
}

fn clip(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(DETAIL_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_owned(),
    }
}

impl Drop for ResponseRecord {
    fn drop(&mut self) {
        let Ok(f) = self.fields.lock() else {
            return;
        };
        if f.stop.is_none() && f.error.is_none() {
            warn!(
                "TRUNCATED: stream ended without a stop reason, upstream hung up or the client walked away"
            );
        }
        // end_turn is upstream's word for "the model finished by itself", and
        // upstream will say it about a sentence that stops mid-clause. Only
        // this one reason gets second-guessed: tool_use and pause_turn are
        // supposed to break off mid-thought, and stop_sequence stops wherever
        // the stop word happened to land.
        let cut = f.stop.as_deref() == Some("end_turn")
            && matches!(f.tail, Some(c) if looks_cut_off(c));
        if cut {
            warn!("TRUNCATED: upstream said end_turn but the reply stops mid-sentence");
        }
        if !f.sent && f.error.is_none() {
            warn!("TRUNCATED: upstream finished but the closing event never reached the client");
        }
        let ok = f.error.is_none()
            && matches!(f.stop.as_deref(), Some(r) if !is_abnormal(r))
            && !cut
            && f.sent;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let out = self.output_tokens.load(Ordering::Relaxed);
        let (think, reply) = split_output(
            out,
            self.think_units.load(Ordering::Relaxed),
            self.reply_units.load(Ordering::Relaxed),
        );
        let line = serde_json::json!({
            "ts": ts,
            "stream": self.stream,
            "format": self.format,
            "model": f.claimed,
            "real": f.real,
            "atis": f.atis,
            "stop": f.stop,
            "ok": ok,
            "cut": cut,
            "sent": f.sent,
            "ms": if self.timed {
                serde_json::json!(self.started.elapsed().as_millis() as u64)
            } else {
                serde_json::Value::Null
            },
            "in": self.input_tokens,
            "out": out,
            "think": think,
            "reply": reply,
            "detail": f.detail,
            "error": f.error,
            "code": f.code,
            "cache_read": f.cache_read,
            "cache_write": f.cache_write,
            "up_in": f.upstream_in,
        });
        append_record(&line.to_string());
    }
}

/// Best effort by design. If the file cannot be written the proxy carries on
/// without it, exactly like every other check in this module.
fn append_record(line: &str) {
    let path = record_path();
    if let Ok(meta) = fs::metadata(path)
        && meta.len() > RECORD_MAX_BYTES
    {
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(".1");
        let _ = fs::rename(path, rotated);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{}", line);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real layout: filler, field tag, length byte, the name, then
    /// the next field's tag (0x38).
    fn synthetic(name: &str) -> String {
        let mut raw = vec![0x11u8; 78];
        raw.push(0x32);
        raw.push(name.len() as u8);
        raw.extend_from_slice(name.as_bytes());
        raw.push(0x38);
        raw.resize(400, 0x22);
        BASE64_STANDARD.encode(raw)
    }

    #[test]
    fn reads_the_exact_model_id() {
        assert_eq!(
            model_id_in_signature(&synthetic("claude-fable-5")),
            Some("claude-fable-5".to_owned())
        );
        assert_eq!(
            model_id_in_signature(&synthetic("claude-opus-4-8")),
            Some("claude-opus-4-8".to_owned())
        );
    }

    #[test]
    fn no_trailing_tag_byte_leaks_into_the_name() {
        let name = model_id_in_signature(&synthetic("claude-fable-5")).unwrap_or_default();
        assert!(!name.ends_with('8'));
        assert_eq!(name.len(), "claude-fable-5".len());
    }

    #[test]
    fn a_swapped_model_does_not_look_like_a_prefix() {
        let actual = model_id_in_signature(&synthetic("claude-fable-5")).unwrap_or_default();
        let claimed = "claude-opus-5";
        assert!(!actual.starts_with(claimed));
        assert!(!claimed.starts_with(actual.as_str()));
    }

    #[test]
    fn a_dated_snapshot_id_still_counts_as_a_match() {
        let actual = model_id_in_signature(&synthetic("claude-opus-4-8")).unwrap_or_default();
        let claimed = "claude-opus-4-8-20260101";
        assert!(claimed.starts_with(actual.as_str()));
    }

    #[test]
    fn short_or_broken_signatures_are_ignored() {
        assert_eq!(model_id_in_signature(""), None);
        assert_eq!(model_id_in_signature("abcd"), None);
        assert_eq!(model_id_in_signature(&"!".repeat(300)), None);
        assert_eq!(model_id_in_signature(&"A".repeat(300)), None);
    }

    fn tail_of(s: &str) -> char {
        s.chars().rev().find(|c| !c.is_whitespace()).unwrap()
    }

    #[test]
    fn a_sentence_that_stops_dead_is_caught() {
        assert!(looks_cut_off(tail_of("他伸手去拿那只")));
        assert!(looks_cut_off(tail_of("她低声说，")));
        assert!(looks_cut_off(tail_of("the door swung open and")));
        assert!(looks_cut_off(tail_of("原因有三：")));
        assert!(looks_cut_off(tail_of("（他停住了（")));
    }

    #[test]
    fn a_finished_sentence_is_left_alone() {
        assert!(!looks_cut_off(tail_of("他伸手去拿那只杯子。")));
        assert!(!looks_cut_off(tail_of("你说什么？")));
        assert!(!looks_cut_off(tail_of("走开！")));
        assert!(!looks_cut_off(tail_of("the door swung open.")));
        assert!(!looks_cut_off(tail_of("*她转过身去*")));
        assert!(!looks_cut_off(tail_of("「我不走」")));
        assert!(!looks_cut_off(tail_of("好吧……")));
        assert!(!looks_cut_off(tail_of("收工 🎉")));
        // Trailing whitespace must not be what gets judged.
        assert!(!looks_cut_off(tail_of("说完了。\n\n")));
    }

    #[test]
    fn the_halves_add_back_up_to_what_upstream_charged() {
        assert_eq!(split_output(1000, 30, 70), (Some(300), Some(700)));
    }

    #[test]
    fn a_long_think_behind_a_short_answer_stops_hiding() {
        let (think, reply) = split_output(16_400, 15_000, 400);
        let (think, reply) = (think.unwrap(), reply.unwrap());
        assert!(think > reply * 10);
        assert_eq!(think + reply, 16_400);
    }

    #[test]
    fn a_reply_with_no_thinking_keeps_the_whole_total() {
        assert_eq!(split_output(500, 0, 120), (Some(0), Some(500)));
    }

    #[test]
    fn nothing_measured_says_nothing_instead_of_zero() {
        assert_eq!(split_output(500, 0, 0), (None, None));
        assert_eq!(split_output(0, 10, 10), (None, None));
    }

    #[test]
    fn rounding_never_loses_or_invents_a_token() {
        for out in [1u64, 3, 7, 999, 1001, 65_537] {
            let (think, reply) = split_output(out, 1, 2);
            assert_eq!(think.unwrap() + reply.unwrap(), out);
        }
    }

    #[test]
    fn thinking_and_reply_are_measured_on_the_same_scale() {
        assert!(count_units("想了很久很久很久，还是决定不说") > count_units("好"));
        assert_eq!(count_units(""), 0);
    }

    #[test]
    fn a_nonsense_length_byte_is_rejected() {
        let mut raw = vec![0x11u8; 78];
        raw.push(0x32);
        raw.push(0xff);
        raw.extend_from_slice(b"claude-fable-5");
        raw.resize(400, 0x22);
        assert_eq!(model_id_in_signature(&BASE64_STANDARD.encode(raw)), None);
    }
}
