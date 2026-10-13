//! 授权登录（OAuth PKCE）：不靠 claude.ai 的 cookie，走 Claude Code CLI 自己的登录流程
//! 拿 access_token / refresh_token。端点、参数、作用域照 gproxy v4.1.5 的 claudecode 渠道。
//!
//! 这样登进来的账号没有网页 cookie，账号表里用一个「合成 cookie」占位当主键
//! （见 `ClewdrCookie::oauth_login`），令牌过期只靠 refresh_token 续；续不动就判失效，
//! 等人回面板重新登一次。

use std::{sync::LazyLock, time::Duration};

use chrono::Utc;
use http::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use moka::sync::Cache;
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::{CLEWDR_CONFIG, ClewdrCookie, CookieStatus, Organization, TokenInfo};

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// 交互式登录要的作用域
const LOGIN_SCOPE: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins";
/// 续期时带的作用域
const REFRESH_SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins";
/// CLI 换令牌、查资料走的是 axios
const AXIOS_USER_AGENT: &str = "axios/1.13.6";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;

/// 发出去还没回来的登录：state -> code_verifier，一刻钟内有效
static PENDING: LazyLock<Cache<String, String>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(32)
        .time_to_live(Duration::from_secs(15 * 60))
        .build()
});

/// 查询串里的值：空格编成 %20，作用域里的空格才不会走样
fn encode_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

fn authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
    let pairs = [
        ("code", "true"),
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", LOGIN_SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

/// 开一次登录：返回（登录页网址，state）。verifier 留在服务器内存里等回填授权码。
pub fn start() -> (String, String) {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let state = CsrfToken::new_random_len(32).secret().to_owned();
    PENDING.insert(state.clone(), verifier.secret().to_owned());
    let client_id = CLEWDR_CONFIG.load().cc_client_id();
    (authorize_url(&client_id, challenge.as_str(), &state), state)
}

/// 用户贴回来的东西：回调页上显示的是 `授权码#state`，也可能只贴了授权码，或者整条回调网址
fn parse_pasted(input: &str) -> (String, Option<String>) {
    let s = input.trim();
    if s.contains("code=") {
        let query = s.split_once('?').map(|(_, q)| q).unwrap_or(s);
        let query = query.split_once('#').map(|(q, _)| q).unwrap_or(query);
        let mut code = None;
        let mut state = None;
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some(code) = code {
            return (code, state);
        }
    }
    match s.split_once('#') {
        Some((code, state)) => (code.trim().to_owned(), Some(state.trim().to_owned())),
        None => (s.to_owned(), None),
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// 令牌端点报错时给人看的一句话，不带任何令牌
fn brief_error(status: http::StatusCode, body: &[u8]) -> String {
    let value: Option<Value> = serde_json::from_slice(body).ok();
    let detail = value
        .as_ref()
        .and_then(|v| {
            let e = v.get("error")?;
            text(e.get("message"))
                .or_else(|| text(v.get("error_description")))
                .or_else(|| text(Some(e)))
                .or_else(|| text(e.get("type")))
        })
        .unwrap_or_default();
    if detail.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}：{}", status.as_u16(), detail)
    }
}

fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    match value.get("error")? {
        Value::String(code) => Some(code.to_ascii_lowercase()),
        Value::Object(e) => e
            .get("code")
            .or_else(|| e.get("type"))
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase),
        _ => None,
    }
}

async fn post_token(client: &wreq::Client, body: Value) -> Result<(http::StatusCode, Vec<u8>), String> {
    let res = client
        .post(TOKEN_URL)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(USER_AGENT, AXIOS_USER_AGENT)
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("连不上令牌端点：{e}"))?;
    let status = res.status();
    let bytes = res
        .bytes()
        .await
        .map_err(|e| format!("读令牌端点的回应失败：{e}"))?;
    Ok((status, bytes.to_vec()))
}

/// 账号资料（邮箱、账号与组织的 uuid），拿不到就算了
async fn fetch_profile(client: &wreq::Client, access_token: &str) -> Option<Value> {
    let res = client
        .get(PROFILE_URL)
        .bearer_auth(access_token)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(USER_AGENT, AXIOS_USER_AGENT)
        .header("anthropic-beta", OAUTH_BETA)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json::<Value>().await.ok()
}

pub struct LoginResult {
    pub status: CookieStatus,
    pub email: Option<String>,
}

/// 收尾：拿授权码换令牌，拼成一条能放进账号表的记录
pub async fn finish(client: &wreq::Client, state: &str, pasted: &str) -> Result<LoginResult, String> {
    let (code, pasted_state) = parse_pasted(pasted);
    if code.is_empty() {
        return Err("授权码是空的".into());
    }
    let state = state.trim();
    if let Some(ps) = pasted_state.as_deref()
        && ps != state
    {
        return Err("这串授权码不是这次登录链接生成的，重新生成链接再登一次".into());
    }
    let Some(verifier) = PENDING.get(state) else {
        return Err("这次登录已经过期（链接生成后一刻钟内有效），重新生成链接再登一次".into());
    };

    let body = json!({
        "grant_type": "authorization_code",
        "client_id": CLEWDR_CONFIG.load().cc_client_id(),
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
        "state": state,
    });
    let (status, bytes) = post_token(client, body).await?;
    if !status.is_success() {
        return Err(format!("换取令牌失败（{}）", brief_error(status, &bytes)));
    }
    // 授权码只能用一次，换成了就把这次登录销掉
    PENDING.invalidate(state);

    let tokens: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("令牌端点的回应看不懂：{e}"))?;
    let access_token = text(tokens.get("access_token")).ok_or("回应里没有 access_token")?;
    let refresh_token = text(tokens.get("refresh_token")).ok_or("回应里没有 refresh_token")?;
    let expires_in = tokens
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECS);
    let refresh_expires_at = tokens
        .get("refresh_token_expires_in")
        .and_then(Value::as_i64)
        .map(|secs| Utc::now().timestamp() + secs.max(0));

    let mut email = text(tokens.get("account").and_then(|a| a.get("email_address")));
    let mut account_uuid = text(tokens.get("account").and_then(|a| a.get("uuid")));
    let mut org_uuid = text(tokens.get("organization").and_then(|o| o.get("uuid")));
    if (email.is_none() || account_uuid.is_none() || org_uuid.is_none())
        && let Some(profile) = fetch_profile(client, &access_token).await
    {
        let account = profile.get("account");
        let org = profile.get("organization");
        email = email
            .or_else(|| text(account.and_then(|a| a.get("email"))))
            .or_else(|| text(account.and_then(|a| a.get("email_address"))));
        account_uuid = account_uuid.or_else(|| text(account.and_then(|a| a.get("uuid"))));
        org_uuid = org_uuid.or_else(|| text(org.and_then(|o| o.get("uuid"))));
    }

    // 同一个账号每次登录都落在同一条记录上：优先拿账号 uuid 当种子
    let seed = account_uuid
        .clone()
        .or_else(|| org_uuid.clone())
        .unwrap_or_else(|| hex::encode(Sha256::digest(refresh_token.as_bytes())));
    let cookie = ClewdrCookie::oauth_login(&seed);
    let mut record = CookieStatus::new(&cookie, None).map_err(|e| format!("内部错误：{e}"))?;
    let expires = Duration::from_secs(expires_in);
    record.token = Some(TokenInfo {
        access_token,
        expires_in: expires,
        organization: Organization {
            uuid: org_uuid.unwrap_or_default(),
        },
        refresh_token,
        expires_at: Utc::now() + expires,
    });
    record.oauth_email = email.clone();
    record.oauth_refresh_expires_at = refresh_expires_at;
    info!(
        "OAuth login finished for {}",
        email.as_deref().unwrap_or("(unknown email)")
    );
    Ok(LoginResult {
        status: record,
        email,
    })
}

pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Duration,
    pub refresh_expires_at: Option<i64>,
}

pub enum RefreshError {
    /// refresh_token 被明确拒了，只能重新登录
    Rejected(String),
    /// 网络之类的临时毛病，账号别动
    Transient(String),
}

/// 拿 refresh_token 续一张新的 access_token
pub async fn refresh(client: &wreq::Client, refresh_token: &str) -> Result<Refreshed, RefreshError> {
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": CLEWDR_CONFIG.load().cc_client_id(),
        "refresh_token": refresh_token,
        "scope": REFRESH_SCOPE,
    });
    let (status, bytes) = post_token(client, body)
        .await
        .map_err(RefreshError::Transient)?;
    if !status.is_success() {
        let code = error_code(&bytes);
        let definitive = status == http::StatusCode::UNAUTHORIZED
            || (status.is_client_error() && code.as_deref() == Some("invalid_grant"));
        let brief = brief_error(status, &bytes);
        if definitive {
            warn!("OAuth refresh rejected: {}", brief);
            return Err(RefreshError::Rejected(brief));
        }
        return Err(RefreshError::Transient(brief));
    }
    let tokens: Value = serde_json::from_slice(&bytes)
        .map_err(|e| RefreshError::Transient(format!("回应看不懂：{e}")))?;
    let access_token = text(tokens.get("access_token"))
        .ok_or_else(|| RefreshError::Transient("回应里没有 access_token".into()))?;
    Ok(Refreshed {
        access_token,
        refresh_token: text(tokens.get("refresh_token")),
        expires_in: Duration::from_secs(
            tokens
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_EXPIRES_IN_SECS),
        ),
        refresh_expires_at: tokens
            .get("refresh_token_expires_in")
            .and_then(Value::as_i64)
            .map(|secs| Utc::now().timestamp() + secs.max(0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_code_with_state() {
        let (code, state) = parse_pasted("  abc123#st4te  ");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("st4te"));
    }

    #[test]
    fn pasted_bare_code() {
        let (code, state) = parse_pasted("abc123");
        assert_eq!(code, "abc123");
        assert!(state.is_none());
    }

    #[test]
    fn pasted_callback_url() {
        let (code, state) =
            parse_pasted("https://platform.claude.com/oauth/code/callback?code=abc123&state=st4te");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("st4te"));
    }

    #[test]
    fn authorize_url_shape() {
        let url = authorize_url("client-x", "chal_lenge", "st4te");
        assert!(url.starts_with(
            "https://claude.com/cai/oauth/authorize?code=true&client_id=client-x&response_type=code"
        ));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"
        ));
        assert!(url.contains("scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference"));
        assert!(url.ends_with("code_challenge=chal_lenge&code_challenge_method=S256&state=st4te"));
    }

    #[test]
    fn synthetic_cookie_roundtrip() {
        let cookie = ClewdrCookie::oauth_login("3f2a9c1e-0000-4000-8000-123456789abc");
        assert!(cookie.is_oauth_login());
        let again = ClewdrCookie::oauth_login("3f2a9c1e-0000-4000-8000-123456789abc");
        assert_eq!(cookie, again);
        // 得过得了账号表原有的格式校验，存进配置再读出来才不会丢
        let parsed: ClewdrCookie = cookie.parse().expect("synthetic cookie must parse");
        assert_eq!(parsed, cookie);
        assert!(parsed.is_oauth_login());
    }
}
