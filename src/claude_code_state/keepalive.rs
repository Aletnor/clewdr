//! 令牌保活：账户闲置时也按到期时间续期 access token。
//! 上游会作废长期未使用的 refresh token（2026-10 实测：闲置 16 小时仍可续期，闲置 47 小时被拒）。
//! 原先续期只在聊天请求和面板查询时顺带进行，账户闲置超过期限后只能重新登录。
//! 本任务定时检查，令牌进入到期前五分钟的窗口即续期，频率与官方客户端一致。
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use super::ClaudeCodeState;
use crate::{config::TokenInfo, error::ClewdrError, services::cookie_actor::CookieActorHandle};

/// 检查间隔，须短于 `TokenInfo::is_expired` 的五分钟提前量，保证在令牌失效前续上
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60);

/// 续期失败（网络不通、上游出错、重新授权被拒等）后，同一账户隔这么久再试。
/// 不退避的话，一个续不上的账户会每轮检查都向上游发请求
const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

/// 启动保活任务
pub fn spawn(handle: CookieActorHandle) {
    tokio::spawn(async move {
        // 已被上游明确拒绝的 refresh token 不再重试；重新登录后令牌更换，自动恢复检查
        let mut rejected: HashSet<String> = HashSet::new();
        // 其余失败按 refresh token 记下一次可以重试的时刻
        let mut retry_at: HashMap<String, Instant> = HashMap::new();
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Ok(status) = handle.get_status().await else {
                continue;
            };
            let accounts: Vec<_> = status.valid.into_iter().chain(status.exhausted).collect();
            let present = |rt: &String| {
                accounts
                    .iter()
                    .any(|c| c.token.as_ref().is_some_and(|t| t.refresh_token == *rt))
            };
            rejected.retain(present);
            retry_at.retain(|rt, _| present(rt));
            for cookie in accounts {
                if !needs_renewal(cookie.token.as_ref(), &rejected, &retry_at, Instant::now()) {
                    continue;
                }
                let Some(refresh_token) = cookie.token.as_ref().map(|t| t.refresh_token.clone())
                else {
                    continue;
                };
                let Ok(mut state) = ClaudeCodeState::from_cookie(handle.clone(), cookie) else {
                    continue;
                };
                match state.refresh_token().await {
                    Ok(()) => {
                        retry_at.remove(&refresh_token);
                        debug!("Token keepalive: token refreshed");
                    }
                    Err(ClewdrError::InvalidCookie { .. }) => {
                        rejected.insert(refresh_token);
                        warn!(
                            "Token keepalive: refresh rejected by upstream, the account needs to be logged in again"
                        );
                    }
                    Err(e) => {
                        retry_at.insert(refresh_token, Instant::now() + RETRY_AFTER);
                        warn!(
                            "Token keepalive: refresh failed, next try in {} minutes: {e}",
                            RETRY_AFTER.as_secs() / 60
                        );
                    }
                }
            }
        }
    });
}

/// 令牌已进入到期窗口，其 refresh token 未被上游拒绝过，且不在失败后的等待期内
fn needs_renewal(
    token: Option<&TokenInfo>,
    rejected: &HashSet<String>,
    retry_at: &HashMap<String, Instant>,
    now: Instant,
) -> bool {
    token.is_some_and(|t| {
        t.is_expired()
            && !rejected.contains(&t.refresh_token)
            && retry_at.get(&t.refresh_token).is_none_or(|at| now >= *at)
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::config::Organization;

    fn token(refresh_token: &str, expires_in_secs: i64) -> TokenInfo {
        TokenInfo {
            access_token: "access".into(),
            expires_in: Duration::from_secs(28800),
            organization: Organization {
                uuid: "org".into(),
            },
            refresh_token: refresh_token.into(),
            expires_at: Utc::now() + chrono::Duration::seconds(expires_in_secs),
        }
    }

    fn due(token: Option<&TokenInfo>, rejected: &HashSet<String>) -> bool {
        needs_renewal(token, rejected, &HashMap::new(), Instant::now())
    }

    #[test]
    fn a_fresh_token_is_left_alone() {
        assert!(!due(Some(&token("r", 3600)), &HashSet::new()));
    }

    #[test]
    fn a_token_about_to_expire_is_renewed() {
        assert!(due(Some(&token("r", 120)), &HashSet::new()));
    }

    #[test]
    fn a_long_expired_token_is_still_renewed() {
        assert!(due(Some(&token("r", -86400)), &HashSet::new()));
    }

    #[test]
    fn a_rejected_refresh_token_is_not_retried() {
        let rejected = HashSet::from(["r".to_string()]);
        assert!(!due(Some(&token("r", -60)), &rejected));
        assert!(due(Some(&token("r2", -60)), &rejected));
    }

    #[test]
    fn a_failed_refresh_waits_before_the_next_try() {
        let now = Instant::now();
        let retry_at = HashMap::from([("r".to_string(), now + RETRY_AFTER)]);
        let t = token("r", -60);
        assert!(!needs_renewal(Some(&t), &HashSet::new(), &retry_at, now));
        assert!(!needs_renewal(Some(&t), &HashSet::new(), &retry_at, now + RETRY_AFTER / 2));
        assert!(needs_renewal(Some(&t), &HashSet::new(), &retry_at, now + RETRY_AFTER));
        assert!(needs_renewal(Some(&token("r2", -60)), &HashSet::new(), &retry_at, now));
    }

    #[test]
    fn an_account_without_a_token_is_skipped() {
        assert!(!due(None, &HashSet::new()));
    }

    #[test]
    fn the_check_runs_inside_the_early_expiry_window() {
        assert!(CHECK_INTERVAL < Duration::from_secs(5 * 60));
    }
}
