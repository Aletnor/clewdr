//! 分流号（atis）。Claude Code 启动时请求 `/api/claude_cli/bootstrap`，响应中的
//! `client_data.atis` 是上游按账户和型号分配的分流号，客户端在之后的每个
//! `/v1/messages` 请求里通过 `x-cc-atis` 头回传。持有分流号只表示该账户进入了上游的
//! 某项实验，不代表一定使用了不同的模型（2026-10-14 实测：claude-opus-5-5 附带与否
//! 回答有差异，claude-sonnet-5-5 未测出差异）。
//!
//! 本模块负责查询、缓存和附带分流号，并为面板提供分流探测。没有分流号的型号不附带此头。

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{StreamExt, stream};
use http::header::USER_AGENT;
use moka::sync::Cache;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::chat::CLAUDE_BETA_BASE;
use crate::{
    config::{CLAUDE_CODE_USER_AGENT, CLEWDR_CONFIG},
    utils::build_http_client,
};

pub(super) const ATIS_HEADER: &str = "x-cc-atis";
/// 分流号缓存时长
const TTL: Duration = Duration::from_secs(30 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// 缓存：(令牌指纹, 型号) -> 分流号。没有分流号的型号同样缓存，避免每次请求都查询
static SLOTS: LazyLock<Cache<(u64, String), Option<String>>> =
    LazyLock::new(|| Cache::builder().max_capacity(256).time_to_live(TTL).build());

/// 缓存键只使用令牌的指纹，不保存令牌本身
fn fingerprint(access_token: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    access_token.hash(&mut hasher);
    hasher.finish()
}

/// 从 bootstrap 响应中取出分流号；只接受可直接写入请求头的可见 ASCII 字符
pub(super) fn pick(doc: &Value) -> Option<String> {
    let atis = doc.get("client_data")?.get("atis")?.as_str()?;
    let usable = !atis.is_empty()
        && atis.len() <= 256
        && atis.bytes().all(|b| (0x21..=0x7e).contains(&b));
    usable.then(|| atis.to_string())
}

/// 查询指定型号的分流号。网络错误、超时或非 2xx 时返回 `Err`，不写入缓存，下次请求时重试
async fn fetch(
    client: &wreq::Client,
    endpoint: &url::Url,
    access_token: &str,
    model: &str,
) -> Result<Option<String>, String> {
    Ok(pick(&fetch_doc(client, endpoint, access_token, model).await?))
}

/// 请求 bootstrap，返回完整响应
async fn fetch_doc(
    client: &wreq::Client,
    endpoint: &url::Url,
    access_token: &str,
    model: &str,
) -> Result<Value, String> {
    let mut url = endpoint
        .join("api/claude_cli/bootstrap")
        .map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("entrypoint", "cli")
        .append_pair("model", model);
    let res = client
        .get(url.to_string())
        .bearer_auth(access_token)
        .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
        .header("anthropic-beta", CLAUDE_BETA_BASE)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("status {}", res.status().as_u16()));
    }
    res.json::<Value>().await.map_err(|e| e.to_string())
}

/// 返回本次请求应附带的分流号；没有分流号或查询失败时返回 `None`，请求照常发送
pub(super) async fn lookup(
    client: &wreq::Client,
    endpoint: &url::Url,
    access_token: &str,
    model: &str,
) -> Option<String> {
    let key = (fingerprint(access_token), model.to_string());
    if let Some(known) = SLOTS.get(&key) {
        return known;
    }
    match fetch(client, endpoint, access_token, model).await {
        Ok(slot) => {
            info!(
                "atis for {model}: {}",
                if slot.is_some() { "got one" } else { "none" }
            );
            SLOTS.insert(key, slot.clone());
            slot
        }
        Err(err) => {
            warn!("atis lookup for {model} failed, sending without it: {err}");
            None
        }
    }
}

/// 分流探测查询的型号列表；响应中 additional_model_options 列出的其他型号会追加查询
const PROBE_MODELS: [&str; 12] = [
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
];
/// 所有型号的响应中都会出现、与分流无关的字段，面板不显示
const PLAIN_KEYS: [&str; 4] = ["atis", "cedar_lagoon", "cedar_basin", "experimentKey"];
/// 分流探测结果缓存五分钟，避免反复打开面板时重复查询上游
static PROBED: LazyLock<Cache<u8, Value>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(1)
        .time_to_live(Duration::from_secs(5 * 60))
        .build()
});

/// 取出响应中额外开放给该账户的型号，去掉 `[1m]` 等后缀
fn offered_models(doc: &Value) -> Vec<String> {
    doc.get("additional_model_options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| o.get("model")?.as_str())
        .map(|m| m.split('[').next().unwrap_or(m).trim().to_string())
        .filter(|m| m.starts_with("claude-"))
        .collect()
}

/// 将逐账户、逐型号的响应汇总为每个型号一行：分配到分流号的账户数、查询成功数、失败数、实验名及附加开关
fn summarize(models: &[String], answers: &[(String, Result<Value, String>)]) -> Vec<Value> {
    let mut rows: Vec<Value> = models
        .iter()
        .map(|model| {
            let mine = answers.iter().filter(|(m, _)| m == model);
            let (mut slots, mut asked, mut failed) = (0, 0, 0);
            let mut experiment = None;
            let mut flags: Vec<String> = Vec::new();
            for (_, answer) in mine {
                let Ok(doc) = answer else {
                    failed += 1;
                    continue;
                };
                asked += 1;
                if pick(doc).is_some() {
                    slots += 1;
                }
                let data = doc.get("client_data").and_then(Value::as_object);
                for (k, v) in data.into_iter().flatten() {
                    if k == "experimentKey" {
                        experiment = experiment.or_else(|| v.as_str().map(str::to_string));
                    } else if !PLAIN_KEYS.contains(&k.as_str()) && !flags.contains(k) {
                        flags.push(k.clone());
                    }
                }
            }
            json!({
                "model": model,
                "slots": slots,
                "asked": asked,
                "failed": failed,
                "experiment": experiment,
                "flags": flags,
            })
        })
        .collect();
    // 分配到分流号的型号排在前面，其余保持原顺序
    rows.sort_by_key(|r| r["slots"].as_u64() == Some(0));
    rows
}

/// 对每个账户、每个型号各请求一次 bootstrap，最多 6 个并发，结果写入 SLOTS
async fn ask_all(
    client: &wreq::Client,
    endpoint: &url::Url,
    tokens: &[String],
    models: &[String],
) -> Vec<(String, Result<Value, String>)> {
    let pairs: Vec<(String, String)> = models
        .iter()
        .flat_map(|m| tokens.iter().map(move |t| (t.clone(), m.clone())))
        .collect();
    stream::iter(pairs)
        .map(move |(token, model)| async move {
            let answer = fetch_doc(client, endpoint, &token, &model).await;
            if let Ok(doc) = &answer {
                SLOTS.insert((fingerprint(&token), model.clone()), pick(doc));
            }
            (model, answer)
        })
        .buffer_unordered(6)
        .collect()
        .await
}

/// 分流探测：用每个账户查询各型号的分流号，结果同时写入 SLOTS，供后续请求直接使用
pub async fn probe(tokens: Vec<String>, expired: usize, force: bool) -> Value {
    if !force && let Some(known) = PROBED.get(&0) {
        return known;
    }
    let (endpoint, proxy) = {
        let cfg = CLEWDR_CONFIG.load();
        (cfg.endpoint(), cfg.wreq_proxy.to_owned())
    };
    let client = match build_http_client(proxy.as_ref()) {
        Ok(c) => c,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let mut tokens = tokens;
    tokens.sort();
    tokens.dedup();

    let mut models: Vec<String> = PROBE_MODELS.iter().map(|m| m.to_string()).collect();
    let mut answers = ask_all(&client, &endpoint, &tokens, &models).await;
    let mut extra: Vec<String> = Vec::new();
    for m in answers.iter().filter_map(|(_, a)| a.as_ref().ok()).flat_map(offered_models) {
        if !models.contains(&m) && !extra.contains(&m) {
            extra.push(m);
        }
    }
    if !extra.is_empty() {
        answers.extend(ask_all(&client, &endpoint, &tokens, &extra).await);
        models.extend(extra);
    }

    let gray: Vec<&str> = answers
        .iter()
        .filter(|(_, a)| a.as_ref().is_ok_and(|d| pick(d).is_some()))
        .map(|(m, _)| m.as_str())
        .collect();
    info!("gray probe over {} token(s): {:?}", tokens.len(), gray);

    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let result = json!({
        "at": at,
        "tokens": tokens.len(),
        "expired": expired,
        "models": summarize(&models, &answers),
    });
    if !tokens.is_empty() {
        PROBED.insert(0, result.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn offered_models_strips_suffixes() {
        let doc = json!({"additional_model_options": [
            {"model": "claude-fable-5-1[1m]"}, {"model": "default"}, {"name": "x"}
        ]});
        assert_eq!(offered_models(&doc), vec!["claude-fable-5-1".to_string()]);
        assert!(offered_models(&json!({"additional_model_options": null})).is_empty());
    }

    #[test]
    fn summarize_counts_slots_and_puts_gray_first() {
        let models = vec!["claude-a".to_string(), "claude-b".to_string()];
        let gray = json!({"client_data": {"atis": "abcd", "experimentKey": "exp_x",
            "per_turn_effort": true, "cedar_basin": "2027"}});
        let plain = json!({"client_data": {"cedar_basin": "2027"}});
        let answers = vec![
            ("claude-a".to_string(), Ok(plain.clone())),
            ("claude-a".to_string(), Err("timeout".to_string())),
            ("claude-b".to_string(), Ok(gray)),
            ("claude-b".to_string(), Ok(plain)),
        ];
        let rows = summarize(&models, &answers);
        assert_eq!(rows[0]["model"], "claude-b");
        assert_eq!(rows[0]["slots"], 1);
        assert_eq!(rows[0]["asked"], 2);
        assert_eq!(rows[0]["experiment"], "exp_x");
        assert_eq!(rows[0]["flags"], json!(["per_turn_effort"]));
        assert_eq!(rows[1]["model"], "claude-a");
        assert_eq!(rows[1]["slots"], 0);
        assert_eq!(rows[1]["failed"], 1);
        assert_eq!(rows[1]["flags"], json!([]));
    }

    #[test]
    fn pick_reads_client_data_atis() {
        let doc = json!({"client_data": {"atis": "0123456789abcdef", "other": true}});
        assert_eq!(pick(&doc).as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn pick_is_none_when_the_model_has_no_slot() {
        assert_eq!(pick(&json!({"client_data": {"other": true}})), None);
        assert_eq!(pick(&json!({"client_data": {"atis": ""}})), None);
        assert_eq!(pick(&json!({"client_data": {"atis": 7}})), None);
        assert_eq!(pick(&json!({})), None);
    }

    #[test]
    fn pick_refuses_what_cannot_go_in_a_header() {
        assert_eq!(pick(&json!({"client_data": {"atis": "a b"}})), None);
        assert_eq!(pick(&json!({"client_data": {"atis": "a\r\nx: y"}})), None);
        assert_eq!(pick(&json!({"client_data": {"atis": "号"}})), None);
    }

    #[test]
    fn fingerprint_tells_tokens_apart() {
        assert_eq!(fingerprint("one"), fingerprint("one"));
        assert_ne!(fingerprint("one"), fingerprint("two"));
    }
}
