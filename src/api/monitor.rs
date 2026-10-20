//! Numbers for the panel's monitor page.
//!
//! These used to come from a separate helper (clewdr_watch.py, mounted beside
//! the panel by a reverse proxy) that read the inspection log and counted. The
//! same counting lives here now, rule for rule, so a bare clewdr answers for
//! itself and the page works wherever the binary runs.
//!
//! Everything is derived from the one-line-per-response log written by
//! `claude_code_state::sigcheck`. That log holds bookkeeping only, so nothing
//! in here can leak a prompt or a reply.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::LazyLock,
};

use axum::Json;
use axum_auth::AuthBearer;
use chrono::{DateTime, FixedOffset, Timelike, Utc};
use regex::Regex;
use serde_json::{Value, json};

use super::error::ApiError;
use crate::{claude_code_state::record_path, config::CLEWDR_CONFIG};

/// How much of the log's tail the page looks at, and how many rows it lists.
const TAIL_BYTES: u64 = 400_000;
const MAX_ROWS: usize = 200;

/// Days are cut at Beijing midnight, the way the page has always shown them.
fn beijing() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("valid offset")
}

/// Per-day, per-model totals kept beside the log. The log rotates and forgets;
/// this file is how the all-time tally survives that.
fn rollup_path() -> PathBuf {
    let mut name = record_path()
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".daily.json");
    record_path().with_file_name(name)
}

fn rotated_path() -> PathBuf {
    let mut name = record_path().as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

fn read_lines(path: &Path, skip_to: u64) -> Vec<Value> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = if skip_to > 0 && file.seek(SeekFrom::Start(skip_to)).is_ok() {
        let mut reader = BufReader::new(file);
        // The seek most likely landed inside a line; drop that broken half.
        let mut half = Vec::new();
        let _ = reader.read_until(b'\n', &mut half);
        reader
    } else {
        BufReader::new(file)
    };
    let mut rows = Vec::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Ok(row) = serde_json::from_slice::<Value>(&raw)
                    && row.is_object()
                {
                    rows.push(row);
                }
            }
        }
    }
    rows
}

fn read_tail(path: &Path, budget: u64) -> Vec<Value> {
    let Ok(size) = fs::metadata(path).map(|m| m.len()) else {
        return Vec::new();
    };
    read_lines(path, size.saturating_sub(budget))
}

/// Only the tail, so the page opens as fast on a fat log as on a thin one.
/// Right after a rotation the new file holds a handful of rows and the rest of
/// today sits in `.1`, so whatever budget the new file left unused is spent on
/// the tail of the old one.
fn read_records() -> Vec<Value> {
    let size = fs::metadata(record_path()).map(|m| m.len()).unwrap_or(0);
    let mut rows = if size < TAIL_BYTES {
        read_tail(&rotated_path(), TAIL_BYTES - size)
    } else {
        Vec::new()
    };
    rows.extend(read_tail(record_path(), TAIL_BYTES));
    rows
}

/// The whole log with its rotated half. The per-model tally needs all of it.
fn read_all_records() -> Vec<Value> {
    let mut rows = read_lines(&rotated_path(), 0);
    rows.extend(read_lines(record_path(), 0));
    rows
}

fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn number(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

fn truthy(row: &Value, key: &str) -> bool {
    match row.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// The category and explanation upstream tucks into its closing event when it
/// refuses. Dug out when present, shrugged off when not.
fn refusal_info(row: &Value) -> (Option<String>, Option<String>) {
    let Some(detail) = text(row, "detail").and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    else {
        return (None, None);
    };
    let details = detail
        .get("delta")
        .and_then(|delta| delta.get("stop_details"))
        .filter(|d| d.is_object())
        .or_else(|| detail.get("stop_details").filter(|d| d.is_object()));
    let Some(details) = details else {
        return (None, None);
    };
    let category = match details.get("category") {
        Some(Value::String(s)) if !s.is_empty() => s.to_owned(),
        Some(other) if !other.is_null() && other != &json!(false) => other.to_string(),
        _ => return (None, None),
    };
    let explanation = details
        .get("explanation")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (Some(category), explanation)
}

fn same_model(claimed: Option<&str>, real: Option<&str>) -> bool {
    match (claimed, real) {
        (Some(claimed), Some(real)) if !claimed.is_empty() && !real.is_empty() => {
            claimed == real || claimed.starts_with(real) || real.starts_with(claimed)
        }
        _ => true,
    }
}

/// The status code this row ended on. Newer rows carry it; older ones get it
/// picked out of the error text; a stream with no code got past HTTP, so 200.
/// Anything else stays blank rather than wearing a made-up number.
fn status(row: &Value) -> Option<String> {
    static IN_TEXT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bcode:\s*(\d{3})\b").unwrap());
    static LEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d{3})\b").unwrap());
    if let Some(code) = number(row, "code") {
        return Some(code.to_string());
    }
    let Some(err) = error_text(row) else {
        return Some("200".to_owned());
    };
    if let Some(found) = IN_TEXT.captures(&err).or_else(|| LEADING.captures(&err)) {
        return Some(found[1].to_owned());
    }
    truthy(row, "stream").then(|| "200".to_owned())
}

fn error_text(row: &Value) -> Option<String> {
    match row.get("error") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.to_owned()),
        Some(Value::Null) | None | Some(Value::String(_)) => None,
        Some(other) => Some(other.to_string()),
    }
}

/// The name upstream gave its own error, carried over as it is.
fn err_type(row: &Value) -> Option<String> {
    static QUOTED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""type":\s*"([^"]+)""#).unwrap());
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-z_]+_error):").unwrap());
    let err = error_text(row)?;
    QUOTED
        .captures(&err)
        .or_else(|| PREFIX.captures(&err))
        .map(|found| found[1].to_owned())
}

fn is_http_failure(code: Option<&str>) -> bool {
    code.is_some_and(|c| c.starts_with('4') || c.starts_with('5'))
}

/// What there is to say beyond the status code, in clewdr's and upstream's
/// own words. Upstream's name for the error goes first: a reply that died
/// midway is both "no stop" and "not sent", but if upstream said overloaded,
/// that is the cause and the other two are what it looked like.
fn flags(row: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let code = status(row);
    if let Some(kind) = err_type(row)
        && !is_http_failure(code.as_deref())
    {
        out.push(kind.strip_suffix("_error").unwrap_or(&kind).to_owned());
    }
    match row.get("stop") {
        None | Some(Value::Null) => {
            if error_text(row).is_none() {
                out.push("no stop".to_owned());
            }
        }
        Some(Value::String(stop)) if stop == "end_turn" => {}
        Some(Value::String(stop)) => out.push(stop.to_owned()),
        Some(other) => out.push(other.to_string()),
    }
    if truthy(row, "cut") {
        out.push("cut".to_owned());
    }
    if row.get("sent") == Some(&Value::Bool(false)) {
        out.push("not sent".to_owned());
    }
    out
}

fn is_bad(row: &Value) -> bool {
    !truthy(row, "ok")
}

/// Which bucket a failed row is counted under at the top of the page.
fn reason_token(row: &Value) -> String {
    let code = status(row);
    if is_http_failure(code.as_deref()) {
        return code.unwrap_or_default();
    }
    flags(row)
        .into_iter()
        .next()
        .or(code)
        .unwrap_or_else(|| "?".to_owned())
}

fn clock(row: &Value) -> DateTime<FixedOffset> {
    DateTime::from_timestamp(number(row, "ts").unwrap_or(0), 0)
        .unwrap_or_default()
        .with_timezone(&beijing())
}

/// Output tokens per second, to one decimal.
fn rate(row: &Value) -> Option<f64> {
    let ms = number(row, "ms").filter(|ms| *ms != 0)?;
    let out = number(row, "out").filter(|out| *out != 0)?;
    let speed = out as f64 * 1000.0 / ms as f64;
    format!("{speed:.1}").parse().ok()
}

/// Upstream bills thinking and reply as one number; newer rows carry the two
/// halves. Older rows do not, and get no invented split.
fn split_out(row: &Value) -> Option<(i64, i64)> {
    let half = |key: &str| {
        let value = row.get(key)?;
        value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
    };
    Some((half("think")?, half("reply")?))
}

/// The claude- prefix and the date tail are the same on every row.
fn short_model(name: Option<&str>) -> String {
    static DATED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-20\d{6}$").unwrap());
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return "?".to_owned();
    };
    let trimmed = name.strip_prefix("claude-").unwrap_or(name);
    DATED.replace(trimmed, "").into_owned()
}

fn row_json(row: &Value) -> Value {
    let code = status(row);
    let marks = flags(row);
    let bad = is_bad(row);
    let (category, why) = refusal_info(row);
    let pair = split_out(row);
    let model = text(row, "model");
    let real = text(row, "real");

    let mut tags = Vec::new();
    if let Some(kind) = err_type(row) {
        tags.push(kind);
    }
    tags.push(match text(row, "stop").filter(|s| !s.is_empty()) {
        Some(stop) => format!("stop_reason {stop}"),
        None => "no stop".to_owned(),
    });
    if truthy(row, "cut") {
        tags.push("cut".to_owned());
    }
    if row.get("sent") == Some(&Value::Bool(false)) {
        tags.push("not sent".to_owned());
    }
    if let Some(category) = &category {
        tags.push(category.to_owned());
    }
    let extra = why
        .filter(|w| !w.is_empty())
        .or_else(|| error_text(row))
        .or_else(|| text(row, "detail").filter(|d| !d.is_empty()).map(str::to_owned));
    // Blocked before a single token is a scan of the request; cut after some
    // output is a scan of the reply.
    let refusal = (text(row, "stop") == Some("refusal")).then(|| {
        json!({
            "category": category,
            "stage": if number(row, "out").unwrap_or(0) != 0 { "output" } else { "input" },
        })
    });
    let sig = match real {
        None => "none",
        Some(_) if same_model(model, real) => "ok",
        Some(_) => "bad",
    };
    json!({
        "refusal": refusal,
        "ts": number(row, "ts").unwrap_or(0),
        "model": row.get("model").cloned().unwrap_or(Value::Null),
        "short": short_model(model),
        "real": row.get("real").cloned().unwrap_or(Value::Null),
        "sig": sig,
        "atis": row.get("atis").cloned().unwrap_or(Value::Null),
        "code": code,
        "ok": !bad,
        "flag": marks.first(),
        "reason": bad.then(|| reason_token(row)),
        "tags": tags,
        "in": number(row, "in").unwrap_or(0),
        "out": number(row, "out").unwrap_or(0),
        "think": pair.map(|p| p.0),
        "reply": pair.map(|p| p.1),
        "ms": row.get("ms").cloned().unwrap_or(Value::Null),
        "rate": rate(row),
        "cache_read": row.get("cache_read").cloned().unwrap_or(Value::Null),
        "cache_write": row.get("cache_write").cloned().unwrap_or(Value::Null),
        "stream": truthy(row, "stream"),
        "format": row.get("format").cloned().unwrap_or(Value::Null),
        "extra": extra,
    })
}

/// requests, failures, output tokens, signature mismatches
type Counts = [u64; 4];
type Days = HashMap<String, HashMap<String, Counts>>;

/// Totals per day and per model. Every day still present in the log is
/// recounted and written over; days that have rotated out of the log stay in
/// the file as they were, so the totals only ever grow.
fn daily_rollup() -> Days {
    let path = rollup_path();
    let mut days: Days = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut fresh: Days = HashMap::new();
    for row in read_all_records() {
        let day = clock(&row).format("%Y-%m-%d").to_string();
        let counts = fresh
            .entry(day)
            .or_default()
            .entry(short_model(text(&row, "model")))
            .or_default();
        counts[0] += 1;
        if is_bad(&row) {
            counts[1] += 1;
        }
        counts[2] += number(&row, "out").unwrap_or(0).max(0) as u64;
        if !same_model(text(&row, "model"), text(&row, "real")) {
            counts[3] += 1;
        }
    }
    days.extend(fresh);
    if let Ok(body) = serde_json::to_vec(&days) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, body).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
    days
}

fn tally(days: &Days, keep: impl Fn(&str) -> bool) -> Vec<Value> {
    let mut sums: HashMap<&str, Counts> = HashMap::new();
    for (day, models) in days {
        if !keep(day) {
            continue;
        }
        for (name, counts) in models {
            let sum = sums.entry(name.as_str()).or_default();
            for (total, n) in sum.iter_mut().zip(counts) {
                *total += n;
            }
        }
    }
    let mut list: Vec<(&str, Counts)> = sums.into_iter().collect();
    // Busiest first; equal counts fall back to the name so the order is stable.
    list.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(a.0.cmp(b.0)));
    list.into_iter()
        .map(|(name, c)| json!({ "name": name, "n": c[0], "bad": c[1], "out": c[2], "swap": c[3] }))
        .collect()
}

fn tally_block(now: DateTime<FixedOffset>) -> Value {
    let days = daily_rollup();
    let today = now.format("%Y-%m-%d").to_string();
    let month = now.format("%Y-%m").to_string();
    json!({
        "since": days.keys().min(),
        "today": tally(&days, |d| d == today),
        "month": tally(&days, |d| d.starts_with(&month)),
        "all": tally(&days, |_| true),
    })
}

fn payload(rows: &[Value]) -> Value {
    let now = Utc::now().with_timezone(&beijing());
    let today: Vec<&Value> = rows
        .iter()
        .filter(|r| clock(r).date_naive() == now.date_naive())
        .collect();
    let bad_today: Vec<&&Value> = today.iter().filter(|r| is_bad(r)).collect();

    // Refusals are split by upstream's category, and each category by whether
    // the request was blocked at the door or the reply was cut midway.
    let mut reasons: Vec<(String, Option<String>, [u64; 3])> = Vec::new();
    for row in &bad_today {
        let key = reason_token(row);
        let category = (key == "refusal").then(|| refusal_info(row).0).flatten();
        let at = match reasons
            .iter()
            .position(|(k, c, _)| *k == key && *c == category)
        {
            Some(at) => at,
            None => {
                reasons.push((key.clone(), category, [0; 3]));
                reasons.len() - 1
            }
        };
        let counts = &mut reasons[at].2;
        counts[0] += 1;
        if key == "refusal" {
            counts[if number(row, "out").unwrap_or(0) != 0 { 2 } else { 1 }] += 1;
        }
    }
    reasons.sort_by(|a, b| {
        b.2[0]
            .cmp(&a.2[0])
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.as_deref().unwrap_or("").cmp(b.1.as_deref().unwrap_or("")))
    });
    let reasons: Vec<Value> = reasons
        .into_iter()
        .map(|(key, category, c)| {
            json!({ "key": key, "category": category, "n": c[0], "input": c[1], "output": c[2] })
        })
        .collect();

    let pairs: Vec<Option<(i64, i64)>> = today.iter().map(|r| split_out(r)).collect();
    let split = !today.is_empty() && pairs.iter().all(Option::is_some);
    let mut spans: Vec<i64> = today
        .iter()
        .filter_map(|r| number(r, "ms").filter(|ms| *ms != 0))
        .collect();
    spans.sort_unstable();

    let cached: Vec<&&Value> = today
        .iter()
        .filter(|r| r.get("cache_read").is_some_and(|v| !v.is_null()))
        .collect();
    let sum = |key: &str| -> i64 { cached.iter().map(|r| number(r, key).unwrap_or(0)).sum() };
    let (read, written, full) = (sum("cache_read"), sum("cache_write"), sum("up_in"));
    let total = read + written + full;

    let swapped: Vec<&Value> = rows
        .iter()
        .filter(|r| !same_model(text(r, "model"), text(r, "real")))
        .collect();

    // Requests per hour today, as [fine, failed], for the bar chart.
    let mut hours = [[0u32; 2]; 24];
    for row in &today {
        hours[clock(row).hour() as usize][usize::from(is_bad(row))] += 1;
    }

    let shown = &rows[rows.len().saturating_sub(MAX_ROWS)..];
    json!({
        "now": now.to_rfc3339(),
        "hours": hours,
        "total": rows.len(),
        "today": {
            "count": today.len(),
            "bad": bad_today.len(),
            "reasons": reasons,
            "out": today.iter().map(|r| number(r, "out").unwrap_or(0)).sum::<i64>(),
            "reply": split.then(|| pairs.iter().flatten().map(|p| p.1).sum::<i64>()),
            "think": split.then(|| pairs.iter().flatten().map(|p| p.0).sum::<i64>()),
            "median_ms": spans.get(spans.len() / 2),
            "cache_pct": (total != 0).then(|| 100 * read / total),
        },
        "swapped": {
            "count": swapped.len(),
            "model": swapped.last().and_then(|r| r.get("model")),
            "real": swapped.last().and_then(|r| r.get("real")),
        },
        "rows": shown.iter().rev().map(row_json).collect::<Vec<_>>(),
        "tally": tally_block(now),
    })
}

/// API endpoint behind the panel's monitor page
pub async fn api_monitor(AuthBearer(t): AuthBearer) -> Result<Json<Value>, ApiError> {
    if !CLEWDR_CONFIG.load().admin_auth(&t) {
        return Err(ApiError::unauthorized());
    }
    // Reading and counting a few hundred kilobytes of log is file work, kept
    // off the threads that are busy streaming replies.
    tokio::task::spawn_blocking(|| payload(&read_records()))
        .await
        .map(Json)
        .map_err(|e| ApiError::internal(format!("监控数据算不出来：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_reply_is_fine_and_says_nothing_extra() {
        let row = json!({"ts": 1, "model": "claude-opus-5-5", "real": "claude-opus-5-5", "stop": "end_turn",
            "ok": true, "sent": true, "cut": false, "code": 200, "stream": true, "ms": 2000, "in": 10, "out": 50,
            "think": 20, "reply": 30});
        assert_eq!(status(&row).as_deref(), Some("200"));
        assert!(flags(&row).is_empty());
        let out = row_json(&row);
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["reason"], Value::Null);
        assert_eq!(out["short"], json!("opus-5-5"));
        assert_eq!(out["sig"], json!("ok"));
        assert_eq!(out["rate"], json!(25.0));
        assert_eq!(out["tags"], json!(["stop_reason end_turn"]));
    }

    #[test]
    fn a_refusal_carries_its_category_and_where_it_was_stopped() {
        let detail = r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"blocked"}}}"#;
        let row = json!({"ts": 1, "model": "claude-opus-5", "stop": "refusal", "ok": false, "sent": true,
            "code": 200, "stream": true, "out": 0, "detail": detail});
        assert_eq!(reason_token(&row), "refusal");
        let out = row_json(&row);
        assert_eq!(out["refusal"], json!({"category": "cyber", "stage": "input"}));
        assert_eq!(out["extra"], json!("blocked"));
        assert_eq!(out["flag"], json!("refusal"));
    }

    #[test]
    fn a_stream_that_just_stopped_reads_as_no_stop_not_sent() {
        let row = json!({"ts": 1, "model": "claude-opus-4-6", "stop": null, "ok": false, "sent": false,
            "code": 200, "stream": true, "out": 0});
        assert_eq!(flags(&row), vec!["no stop", "not sent"]);
        assert_eq!(reason_token(&row), "no stop");
    }

    #[test]
    fn an_http_failure_is_filed_under_its_code() {
        let row = json!({"ts": 1, "model": "claude-3-opus-20240229", "stop": null, "ok": false, "sent": true,
            "code": 404, "stream": false,
            "error": "Http error: code: 404 Not Found, body: {\n  \"message\": \"model: x\",\n  \"type\": \"not_found_error\"\n}"});
        assert_eq!(reason_token(&row), "404");
        assert_eq!(err_type(&row).as_deref(), Some("not_found_error"));
        assert!(flags(&row).is_empty());
        assert_eq!(short_model(text(&row, "model")), "3-opus");
    }

    #[test]
    fn an_old_row_without_a_code_gets_it_from_the_error_text() {
        let row = json!({"ts": 1, "stream": false, "ok": false, "error": "overloaded_error: Overloaded"});
        assert_eq!(status(&row), None);
        assert_eq!(flags(&row)[0], "overloaded");
        let streamed = json!({"ts": 1, "stream": true, "ok": false, "error": "overloaded_error: Overloaded"});
        assert_eq!(status(&streamed).as_deref(), Some("200"));
    }

    #[test]
    fn a_dated_snapshot_is_the_same_model_and_a_swap_is_not() {
        assert!(same_model(Some("claude-opus-4-5"), Some("claude-opus-4-5-20251101")));
        assert!(same_model(Some("claude-opus-5"), None));
        assert!(!same_model(Some("claude-opus-5"), Some("claude-fable-5")));
    }
}
