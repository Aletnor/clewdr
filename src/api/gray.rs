//! 面板分流探测接口：用所有账户令牌，查询上游为各型号分配的分流号。已过期的令牌先续期再参与探测
use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use super::error::ApiError;
use crate::{
    claude_code_state::{ClaudeCodeState, atis_probe},
    config::CookieStatus,
    services::cookie_actor::CookieActorHandle,
};

#[derive(Deserialize)]
pub struct GrayQuery {
    #[serde(default)]
    refresh: bool,
}

/// `GET /api/gray`；`?refresh=true` 时忽略五分钟缓存，重新查询
pub async fn api_gray(
    State(s): State<CookieActorHandle>,
    Query(q): Query<GrayQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = s
        .get_status()
        .await
        .map_err(|e| ApiError::internal(format!("Failed to get cookie status: {e}")))?;
    let mut expired = 0;
    let mut tokens = Vec::new();
    for cookie in status.valid.into_iter().chain(status.exhausted) {
        let Some(token) = cookie.token.as_ref() else {
            continue;
        };
        if !token.is_expired() {
            tokens.push(token.access_token.clone());
            continue;
        }
        match renew(&s, cookie).await {
            Some(t) => tokens.push(t),
            None => expired += 1,
        }
    }
    Ok(Json(atis_probe(tokens, expired, q.refresh).await))
}

/// 续期一个已过期的令牌。成功时新令牌已交还账户池，返回新的 access token；
/// 失败时不改动账户状态，由下一次聊天请求按原有规则处理
async fn renew(handle: &CookieActorHandle, cookie: CookieStatus) -> Option<String> {
    let mut state = ClaudeCodeState::from_cookie(handle.clone(), cookie).ok()?;
    if let Err(e) = state.refresh_token().await {
        warn!("Gray probe: token refresh failed: {e}");
        return None;
    }
    state.cookie?.token.map(|t| t.access_token)
}
