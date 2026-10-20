use std::collections::HashSet;

use axum::{
    Json,
    response::{IntoResponse, Sse, sse::Event as SseEvent},
};
use colored::Colorize;
use eventsource_stream::Eventsource;
use futures::TryStreamExt;
use http::header::{ACCEPT, USER_AGENT};
use snafu::{GenerateImplicitData, ResultExt};
use tracing::{Instrument, error, info, warn};
use wreq::Method;

use crate::{
    claude_code_state::{ClaudeCodeState, TokenStatus},
    config::{CLAUDE_CODE_USER_AGENT, CLEWDR_CONFIG, ModelFamily, Reason},
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    services::cookie_actor::CookieActorHandle,
    types::claude::{CountMessageTokensResponse, CreateMessageParams},
};

pub(super) const CLAUDE_BETA_BASE: &str = "oauth-2025-04-20";
const CLAUDE_BETA_CONTEXT_1M_TOKEN: &str = "context-1m-2025-08-07";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub(super) const CLAUDE_API_VERSION: &str = "2023-06-01";

impl ClaudeCodeState {
    /// Attempts to send a chat message to Claude API with retry mechanism
    ///
    /// This method handles the complete chat flow including:
    /// - Request preparation and logging
    /// - Cookie management for authentication
    /// - Executing the chat request with automatic retries on failure
    /// - Response transformation according to the specified API format
    /// - Error handling and cleanup
    ///
    /// The method implements a sophisticated retry mechanism to handle transient failures,
    /// and manages conversation cleanup to prevent resource leaks. It also includes
    /// performance tracking to measure response times.
    ///
    /// # Arguments
    /// * `p` - The client request body containing messages and configuration
    ///
    /// # Returns
    /// * `Result<axum::response::Response, ClewdrError>` - Formatted response or error
    pub async fn try_chat(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        // The last account-wide rate limit met on the way, so that when every
        // account turns out to be benched the client hears upstream's own words.
        let mut last_limit: Option<ClewdrError> = None;
        // The last 529 met on the way, handed to the client if every try ends in one.
        let mut last_overload: Option<ClewdrError> = None;
        let max_retries = CLEWDR_CONFIG.load().max_retries;
        for i in 0..max_retries + 1 {
            if i > 0 {
                info!("[RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = match state.request_cookie().await {
                Ok(cookie) => cookie,
                Err(ClewdrError::NoCookieAvailable) => {
                    return Err(match last_limit.take() {
                        Some(limit) => limit,
                        None => self.cooling_down_error().await,
                    });
                }
                Err(e) => return Err(e),
            };
            let retry = async {
                match state.check_token() {
                    TokenStatus::None => {
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Expired => {
                        info!("Token expired, refreshing token");
                        state.refresh_token().await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with request");
                    }
                }
                let Some(access_token) = state.cookie.as_ref().and_then(|c| c.token.to_owned())
                else {
                    return Err(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    });
                };
                let used = access_token.access_token.to_owned();
                let sent = state.send_chat(used.clone(), p.to_owned()).await;
                if sent.is_ok() || !state.token_rejected {
                    return sent;
                }
                // Upstream said 401 to a token that is not due yet. It used to
                // mean the account was written off on the spot. A token can die
                // early for reasons that a refresh cures (it was rotated
                // elsewhere, or revoked while the refresh token still stands),
                // so spend one refresh on it first. Only when the refresh itself
                // is turned down, or the fresh token is refused too, is the
                // account given up.
                state.token_rejected = false;
                info!("Upstream answered 401 to a token that is not due yet, trying one refresh");
                state.refresh_rejected_token(&used).await?;
                let fresh = state
                    .cookie
                    .as_ref()
                    .and_then(|c| c.token.as_ref())
                    .map(|t| t.access_token.to_owned());
                match fresh {
                    Some(fresh) if fresh != used => state.send_chat(fresh, p).await,
                    _ => sent,
                }
            }
            .instrument(tracing::info_span!(
                "claude_code",
                "cookie" = cookie.cookie.ellipse()
            ));
            match retry.await {
                Ok(res) => {
                    return Ok(res);
                }
                Err(e) => {
                    error!(
                        "[{}] {}",
                        state.cookie.as_ref().unwrap().cookie.ellipse().green(),
                        e
                    );
                    if let ClewdrError::InvalidCookie { reason } = e {
                        state.return_cookie(Some(reason.to_owned())).await;
                        continue;
                    }
                    // Upstream being overloaded is nobody's fault here: not the
                    // account's, not the request's. Wait a little and try again
                    // instead of handing the 529 straight to the client.
                    if Self::is_overloaded(&e) && i < max_retries {
                        let wait = 1500 * (i as u64 + 1);
                        warn!("Upstream overloaded (529), waiting {wait} ms before another try");
                        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                        last_overload = Some(e);
                        continue;
                    }
                    // A limit on the whole subscription benches the account until
                    // it resets. Any other 429 belongs to one model or one kind of
                    // request: the account stays, the client gets the 429 as it is.
                    if let Some(reset_at) = Self::account_wide_limit(&e) {
                        state
                            .return_cookie(Some(Reason::TooManyRequest(reset_at)))
                            .await;
                        last_limit = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last_limit
            .or(last_overload)
            .unwrap_or(ClewdrError::TooManyRetries))
    }

    /// Upstream's "overloaded" answer, the one worth waiting out.
    fn is_overloaded(error: &ClewdrError) -> bool {
        matches!(error, ClewdrError::ClaudeHttpError { code, .. } if code.as_u16() == 529)
    }

    /// The reset time, when upstream says the limit that was hit covers the
    /// whole subscription (the five hour or the seven day window) rather than
    /// one model's own allowance.
    fn account_wide_limit(error: &ClewdrError) -> Option<i64> {
        match error {
            ClewdrError::RateLimited {
                reset_at,
                claim: Some(claim),
                ..
            } if matches!(claim.as_str(), "five_hour" | "seven_day") => Some(*reset_at),
            _ => None,
        }
    }

    /// What to tell the client when no account can be handed out. If accounts
    /// are sitting out a rate limit, say so and say until when, as a 429.
    async fn cooling_down_error(&self) -> ClewdrError {
        let Ok(status) = self.cookie_actor_handle.get_status().await else {
            return ClewdrError::NoCookieAvailable;
        };
        let Some(reset_at) = status.exhausted.iter().filter_map(|c| c.reset_time).min() else {
            return ClewdrError::NoCookieAvailable;
        };
        let beijing = chrono::FixedOffset::east_opt(8 * 3600).expect("valid offset");
        let when = chrono::DateTime::from_timestamp(reset_at, 0)
            .map(|t| t.with_timezone(&beijing).format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| reset_at.to_string());
        ClewdrError::RateLimited {
            reset_at,
            claim: None,
            inner: crate::error::ClaudeErrorBody {
                message: serde_json::json!(format!(
                    "账号在冷却中（上游限流），北京时间 {when} 恢复"
                )),
                r#type: "rate_limit_error".to_string(),
                code: Some(429),
            },
        }
    }

    pub async fn send_chat(
        &mut self,
        access_token: String,
        mut p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        let (base_model, requested_1m) = match p.model.strip_suffix("-1M") {
            Some(stripped) => (stripped.to_string(), true),
            None => (p.model.clone(), false),
        };
        p.model = base_model;

        let attempts = Self::context_1m_attempts(requested_1m);
        let model_family = Self::classify_model(&p.model);
        for (idx, use_context_1m) in attempts.iter().copied().enumerate() {
            match self
                .execute_claude_request(&access_token, &p, use_context_1m)
                .await
            {
                Ok(response) => {
                    return self.handle_success_response(response, model_family).await;
                }
                Err(err) => {
                    let is_last_attempt = idx + 1 == attempts.len();
                    if use_context_1m && !is_last_attempt && Self::is_context_1m_refused(&err) {
                        warn!("1M context refused, retrying without the 1M header");
                        continue;
                    }
                    super::sigcheck::record_failure(
                        self.api_format.to_string().as_str(),
                        &p.model,
                        &err.to_string(),
                        Self::upstream_code(&err),
                    );
                    return Err(err);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    async fn execute_claude_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
        use_context_1m: bool,
    ) -> Result<wreq::Response, ClewdrError> {
        let beta_header = Self::merge_anthropic_beta_header(
            self.anthropic_beta_header.as_deref(),
            use_context_1m,
        );
        // 面板「分流号」开关关闭时，不查询也不附带分流号
        let atis = if CLEWDR_CONFIG.load().atis {
            super::atis::lookup(&self.client, &self.endpoint, access_token, &body.model).await
        } else {
            None
        };
        self.atis_sent = Some(atis.is_some());
        let mut request = self
            .client
            .post(
                self.endpoint
                    .join("v1/messages")
                    .expect("Url parse error")
                    .to_string(),
            )
            .bearer_auth(access_token)
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION);
        if let Some(atis) = atis.as_deref() {
            request = request.header(super::atis::ATIS_HEADER, atis);
        }
        let response = request
            .json(body)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to send chat message",
            })?;
        // Remember a 401 before the status is folded into `Reason::Null`,
        // which a refused organisation (403) is folded into as well.
        self.token_rejected = response.status().as_u16() == 401;
        response.check_claude_code().await
    }

    async fn persist_count_tokens_allowed(&mut self, value: bool) {
        if let Some(cookie) = self.cookie.as_mut() {
            if cookie.count_tokens_allowed == Some(value) {
                return;
            }
            cookie.set_count_tokens_allowed(Some(value));
            let cloned = cookie.clone();
            if let Err(err) = self.cookie_actor_handle.return_cookie(cloned, None).await {
                warn!("Failed to persist count_tokens permission: {}", err);
            }
        }
    }

    pub async fn fetch_usage_metrics(&mut self) -> Result<serde_json::Value, ClewdrError> {
        match self.check_token() {
            TokenStatus::None => {
                let org = self.get_organization().await?;
                let code = self.exchange_code(&org).await?;
                self.exchange_token(code).await?;
            }
            TokenStatus::Expired => {
                self.refresh_token().await?;
            }
            TokenStatus::Valid => {}
        }

        let access_token = self
            .cookie
            .as_ref()
            .and_then(|c| c.token.as_ref())
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No access token available",
            })?
            .access_token
            .to_owned();

        self.client
            .request(Method::GET, CLAUDE_USAGE_URL)
            .bearer_auth(access_token)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", CLAUDE_BETA_BASE)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch usage metrics",
            })?
            .check_claude()
            .await?
            .json::<serde_json::Value>()
            .await
            .context(WreqSnafu {
                msg: "Failed to parse usage metrics response",
            })
    }

    pub async fn try_count_tokens(
        &mut self,
        p: CreateMessageParams,
        for_web: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        for i in 0..CLEWDR_CONFIG.load().max_retries + 1 {
            if i > 0 {
                info!("[TOKENS][RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.request_cookie().await?;
            let web_attempt_allowed = CLEWDR_CONFIG.load().enable_web_count_tokens;
            let cookie_disallows = matches!(cookie.count_tokens_allowed, Some(false));
            if cookie_disallows || (for_web && !web_attempt_allowed) {
                if cookie_disallows {
                    state.persist_count_tokens_allowed(false).await;
                }
                return Ok(Self::local_count_tokens_response(&p));
            }
            let retry = async {
                match state.check_token() {
                    TokenStatus::None => {
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Expired => {
                        info!("Token expired, refreshing token");
                        state.refresh_token().await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with count_tokens");
                    }
                }
                let Some(access_token) = state.cookie.as_ref().and_then(|c| c.token.to_owned())
                else {
                    return Err(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    });
                };
                state
                    .perform_count_tokens(access_token.access_token.to_owned(), p, for_web)
                    .await
            }
            .instrument(tracing::info_span!(
                "claude_code_tokens",
                "cookie" = cookie.cookie.ellipse()
            ));
            match retry.await {
                Ok(res) => {
                    return Ok(res);
                }
                Err(e) => {
                    error!(
                        "[{}][TOKENS] {}",
                        state.cookie.as_ref().unwrap().cookie.ellipse().green(),
                        e
                    );
                    if let ClewdrError::InvalidCookie { reason } = e {
                        state.return_cookie(Some(reason.to_owned())).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    async fn perform_count_tokens(
        &mut self,
        access_token: String,
        mut p: CreateMessageParams,
        allow_fallback: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        p.stream = Some(false);
        let (base_model, requested_1m) = match p.model.strip_suffix("-1M") {
            Some(stripped) => (stripped.to_string(), true),
            None => (p.model.clone(), false),
        };
        p.model = base_model;

        let attempts = Self::context_1m_attempts(requested_1m);
        for (idx, use_context_1m) in attempts.iter().copied().enumerate() {
            match self
                .execute_claude_count_tokens_request(&access_token, &p, use_context_1m)
                .await
            {
                Ok(response) => {
                    self.persist_count_tokens_allowed(true).await;
                    let (resp, _) = Self::materialize_non_stream_response(
                        response,
                        self.api_format.to_string().as_str(),
                        self.atis_sent,
                    )
                    .await?;
                    return Ok(resp);
                }
                Err(err) => {
                    let is_last_attempt = idx + 1 == attempts.len();
                    if use_context_1m && !is_last_attempt && Self::is_context_1m_refused(&err) {
                        warn!("1M context refused in count_tokens, retrying without the 1M header");
                        continue;
                    }

                    if Self::is_count_tokens_unauthorized(&err) {
                        self.persist_count_tokens_allowed(false).await;
                        if allow_fallback {
                            return Ok(Self::local_count_tokens_response(&p));
                        }
                    }
                    return Err(err);
                }
            }
        }

        Err(ClewdrError::TooManyRetries)
    }

    async fn handle_success_response(
        &mut self,
        response: wreq::Response,
        model_family: ModelFamily,
    ) -> Result<axum::response::Response, ClewdrError> {
        if !self.stream {
            let (resp, usage_pair) =
                Self::materialize_non_stream_response(
                    response,
                    self.api_format.to_string().as_str(),
                    self.atis_sent,
                )
                    .await?;
            let (input, output) = usage_pair.unwrap_or((self.usage.input_tokens as u64, 0));
            self.persist_usage_totals(input, output, model_family).await;
            Ok(resp)
        } else {
            // Stream pass-through while accumulating output token usage from message_delta events
            return self.forward_stream_with_usage(response, model_family).await;
        }
    }

    async fn persist_usage_totals(&mut self, input: u64, output: u64, family: ModelFamily) {
        if input == 0 && output == 0 {
            return;
        }
        if let Some(cookie) = self.cookie.as_mut() {
            // Lazy boundary refresh if due, then reset period counters and start fresh
            Self::update_cookie_boundaries_if_due(cookie, &self.cookie_actor_handle).await;
            cookie.add_and_bucket_usage(input, output, family);
            let cloned = cookie.clone();
            if let Err(err) = self.cookie_actor_handle.return_cookie(cloned, None).await {
                warn!("Failed to persist usage statistics: {}", err);
            }
        }
    }

    async fn forward_stream_with_usage(
        &mut self,
        response: wreq::Response,
        family: ModelFamily,
    ) -> Result<axum::response::Response, ClewdrError> {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };

        let input_tokens = self.usage.input_tokens as u64;
        let output_sum = Arc::new(AtomicU64::new(0));
        let handle = self.cookie_actor_handle.clone();
        let cookie = self.cookie.clone();

        let osum = output_sum.clone();
        let mut claimed_model: Option<String> = None;
        let mut signature_checked = false;
        let upstream_status = response.status().as_u16();
        let record = Arc::new(super::sigcheck::ResponseRecord::for_stream(
            self.api_format.to_string().as_str(),
            input_tokens,
            output_sum.clone(),
        ));
        record.note_status(upstream_status);
        if let Some(carried) = self.atis_sent {
            record.note_atis(carried);
        }
        let record_on_error = record.clone();
        let stream = response.bytes_stream().eventsource().map_ok(move |event| {
            // accumulate output tokens from message_delta usage if present
            if let Ok(parsed) =
                serde_json::from_str::<crate::types::claude::StreamEvent>(&event.data)
            {
                match parsed {
                    crate::types::claude::StreamEvent::MessageDelta {
                        ref delta,
                        ref usage,
                    } => {
                        if let Some(u) = usage {
                            osum.fetch_add(u.output_tokens as u64, Ordering::Relaxed);
                        }
                        if let Some(reason) = delta.stop_reason.as_ref() {
                            record.note_reason(reason, &event.data);
                        }
                    }
                    crate::types::claude::StreamEvent::Error { ref error } => {
                        record.note_error_event(&error.type_, &error.message);
                    }
                    crate::types::claude::StreamEvent::MessageStart { ref message } => {
                        claimed_model = Some(message.model.to_owned());
                        record.note_claimed_model(&message.model);
                        // The typed Usage has no cache fields, so read them
                        // straight off the raw event. Once per response.
                        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.data)
                            && let Some(usage) = raw.pointer("/message/usage")
                        {
                            record.note_cache_usage(usage);
                        }
                    }
                    crate::types::claude::StreamEvent::ContentBlockDelta {
                        delta:
                            crate::types::claude::ContentBlockDelta::TextDelta { ref text },
                        ..
                    } => {
                        record.note_text(text);
                    }
                    // Upstream charges for the thinking chain inside the same
                    // output_tokens as the reply, so it gets weighed here or a
                    // one word answer after a long think reads as a long answer.
                    crate::types::claude::StreamEvent::ContentBlockDelta {
                        delta:
                            crate::types::claude::ContentBlockDelta::ThinkingDelta {
                                ref thinking,
                            },
                        ..
                    } => {
                        record.note_think(thinking);
                    }
                    crate::types::claude::StreamEvent::ContentBlockDelta {
                        delta:
                            crate::types::claude::ContentBlockDelta::InputJsonDelta {
                                ref partial_json,
                            },
                        ..
                    } => {
                        record.note_tool_input(partial_json);
                    }
                    crate::types::claude::StreamEvent::ContentBlockDelta {
                        delta:
                            crate::types::claude::ContentBlockDelta::SignatureDelta {
                                ref signature,
                            },
                        ..
                    } => {
                        if !signature_checked
                            && let Some(claimed) = claimed_model.as_deref()
                        {
                            signature_checked = true;
                            record.note_signature(claimed, signature);
                        }
                    }
                    crate::types::claude::StreamEvent::MessageStop => {
                        record.note_sent();
                        // on stream completion, persist totals asynchronously
                        if let (Some(cookie), handle) = (cookie.clone(), handle.clone()) {
                            let total_out = osum.load(Ordering::Relaxed);
                            let mut c = cookie.clone();
                            tokio::spawn(async move {
                                // Update period boundaries if needed, then accumulate
                                ClaudeCodeState::update_cookie_boundaries_if_due(&mut c, &handle)
                                    .await;
                                c.add_and_bucket_usage(input_tokens, total_out, family);
                                let _ = handle.return_cookie(c, None).await;
                            });
                        }
                    }
                    _ => {}
                }
            } else if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.data) {
                if raw.get("type").and_then(|t| t.as_str()) == Some("message_delta")
                    && let Some(reason) =
                        raw.pointer("/delta/stop_reason").and_then(|r| r.as_str())
                {
                    record.note_reason_str(reason, &event.data);
                }
            }
            // mirror upstream SSE event unchanged
            let e = SseEvent::default().event(event.event).id(event.id);
            let e = if let Some(retry) = event.retry {
                e.retry(retry)
            } else {
                e
            };
            e.data(event.data)
        })
        .inspect_err(move |e| record_on_error.note_transport_error(&e.to_string()));

        Ok(Sse::new(stream)
            .keep_alive(Default::default())
            .into_response())
    }

    async fn materialize_non_stream_response(
        response: wreq::Response,
        format: &str,
        atis: Option<bool>,
    ) -> Result<(axum::response::Response, Option<(u64, u64)>), ClewdrError> {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.context(WreqSnafu {
            msg: "Failed to read Claude response body",
        })?;
        let usage = Self::extract_usage_from_bytes(&bytes);
        Self::inspect_response_bytes(&bytes, format, status.as_u16(), atis);

        let mut builder = http::Response::builder().status(status);
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let response =
            builder
                .body(axum::body::Body::from(bytes))
                .map_err(|e| ClewdrError::HttpError {
                    loc: snafu::Location::generate(),
                    source: e,
                })?;
        Ok((response, usage))
    }

    fn inspect_response_bytes(bytes: &[u8], format: &str, code: u16, atis: Option<bool>) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return;
        };
        // An error body has its own shape and no ending to report, so it
        // gets a row of its own rather than vanishing. Anything else that
        // lands here is a token count or similar bookkeeping, not a reply.
        match value.get("type").and_then(|t| t.as_str()) {
            Some("message") => {}
            Some("error") => {
                super::sigcheck::record_failure(format, "", &value.to_string(), Some(code));
                return;
            }
            _ => return,
        }
        let usage = value.get("usage");
        let token_count = |name: &str| {
            usage
                .and_then(|u| u.get(name))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };
        let record = super::sigcheck::ResponseRecord::for_single(
            format,
            token_count("input_tokens"),
            token_count("output_tokens"),
        );
        record.note_status(code);
        if let Some(carried) = atis {
            record.note_atis(carried);
        }
        if let Some(usage) = usage {
            record.note_cache_usage(usage);
        }
        if let Some(reason) = value.get("stop_reason").and_then(|r| r.as_str()) {
            // Everything except the reply body, so the record gains the
            // extra fields without gaining the conversation.
            let mut trimmed = value.clone();
            if let Some(obj) = trimmed.as_object_mut() {
                obj.remove("content");
            }
            record.note_reason_str(reason, &trimmed.to_string());
        }
        let Some(claimed) = value.get("model").and_then(|m| m.as_str()) else {
            return;
        };
        record.note_claimed_model(claimed);
        let Some(blocks) = value.get("content").and_then(|c| c.as_array()) else {
            return;
        };
        for block in blocks {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                record.note_text(text);
            }
            if let Some(thinking) = block.get("thinking").and_then(|t| t.as_str()) {
                record.note_think(thinking);
            }
            if let Some(input) = block.get("input") {
                record.note_tool_input(&input.to_string());
            }
        }
        for block in blocks {
            if let Some(signature) = block.get("signature").and_then(|s| s.as_str()) {
                record.note_signature(claimed, signature);
                return;
            }
        }
    }

    fn extract_usage_from_bytes(bytes: &[u8]) -> Option<(u64, u64)> {
        // Prefer explicit usage if present
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
            && let Some(usage) = value.get("usage")
        {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)));
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)));
            if let (Some(i), Some(o)) = (input, output) {
                return Some((i, o));
            }
        }

        // Fallback: estimate output tokens from the Claude response content
        if let Ok(parsed) =
            serde_json::from_slice::<crate::types::claude::CreateMessageResponse>(bytes)
        {
            let output_tokens = parsed.count_tokens() as u64;
            // Input tokens already computed earlier and present in self.usage; only estimate output here
            return Some((0, output_tokens));
        }
        None
    }

    async fn execute_claude_count_tokens_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
        use_context_1m: bool,
    ) -> Result<wreq::Response, ClewdrError> {
        let beta_header = Self::merge_anthropic_beta_header(
            self.anthropic_beta_header.as_deref(),
            use_context_1m,
        );
        self.client
            .post(
                self.endpoint
                    .join("v1/messages/count_tokens")
                    .expect("Url parse error")
                    .to_string(),
            )
            .bearer_auth(access_token)
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .json(body)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to call Claude count_tokens",
            })?
            .check_claude_code()
            .await
    }

    fn merge_anthropic_beta_header(extra: Option<&str>, use_context_1m: bool) -> String {
        let mut seen = HashSet::new();
        let mut merged = Vec::new();
        let mut push = |token: &str| {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return;
            }
            let key = trimmed.to_ascii_lowercase();
            if !use_context_1m && key == CLAUDE_BETA_CONTEXT_1M_TOKEN {
                return;
            }
            if seen.insert(key) {
                merged.push(trimmed.to_string());
            }
        };

        push(CLAUDE_BETA_BASE);
        if use_context_1m {
            push(CLAUDE_BETA_CONTEXT_1M_TOKEN);
        }
        if let Some(extra) = extra {
            for token in extra.split(',') {
                push(token);
            }
        }
        merged.join(",")
    }

    /// The long context beta header goes out only when the client asked for it
    /// with a `-1M` model suffix, and then with a plain retry behind it. It used
    /// to be probed on every Sonnet 4.x / Opus 4.6 request, from the days when
    /// that header was the only way to get the long window on those models.
    fn context_1m_attempts(requested_1m: bool) -> Vec<bool> {
        if requested_1m {
            vec![true, false]
        } else {
            vec![false]
        }
    }

    fn classify_model(model: &str) -> ModelFamily {
        let m = model.to_ascii_lowercase();
        if m.contains("opus") {
            ModelFamily::Opus
        } else if m.contains("sonnet") {
            ModelFamily::Sonnet
        } else {
            ModelFamily::Other
        }
    }

    // ---------------------------------------------
    // Lazy boundary refresh (no timers, fetch-on-due)
    // ---------------------------------------------
    async fn update_cookie_boundaries_if_due(
        cookie: &mut crate::config::CookieStatus,
        handle: &crate::services::cookie_actor::CookieActorHandle,
    ) {
        let now = chrono::Utc::now().timestamp();
        const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60; // 5h
        const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60; // 7d

        let tracked = |flag: Option<bool>| flag == Some(true);
        let unknown = |flag: Option<bool>| flag.is_none();
        let due = |ts: Option<i64>| ts.map(|t| now >= t).unwrap_or(false);

        let session_tracked = tracked(cookie.session_has_reset);
        let weekly_tracked = tracked(cookie.weekly_has_reset);
        let sonnet_tracked = tracked(cookie.weekly_sonnet_has_reset);
        let opus_tracked = tracked(cookie.weekly_opus_has_reset);

        let session_due = session_tracked && due(cookie.session_resets_at);
        let weekly_due = weekly_tracked && due(cookie.weekly_resets_at);
        let sonnet_due = sonnet_tracked && due(cookie.weekly_sonnet_resets_at);
        let opus_due = opus_tracked && due(cookie.weekly_opus_resets_at);

        let need_probe_unknown = unknown(cookie.session_has_reset)
            || unknown(cookie.weekly_has_reset)
            || unknown(cookie.weekly_sonnet_has_reset)
            || unknown(cookie.weekly_opus_has_reset);
        let any_due = session_due || weekly_due || sonnet_due || opus_due;

        if !(need_probe_unknown || any_due) {
            return;
        }

        cookie.resets_last_checked_at = Some(now);
        if let Some((sess, week, opus, sonnet)) = Self::fetch_usage_resets(cookie, handle).await {
            // Unknown -> decide track/not-track
            if unknown(cookie.session_has_reset) {
                cookie.session_has_reset = Some(sess.is_some());
            }
            if unknown(cookie.weekly_has_reset) {
                cookie.weekly_has_reset = Some(week.is_some());
            }
            if unknown(cookie.weekly_sonnet_has_reset) {
                cookie.weekly_sonnet_has_reset = Some(sonnet.is_some());
            }
            if unknown(cookie.weekly_opus_has_reset) {
                cookie.weekly_opus_has_reset = Some(opus.is_some());
            }

            // Handle due tracked windows: reset usage then update boundaries if provided
            if session_due {
                cookie.session_usage = crate::config::UsageBreakdown::default();
            }
            if weekly_due {
                cookie.weekly_usage = crate::config::UsageBreakdown::default();
            }
            if sonnet_due {
                cookie.weekly_sonnet_usage = crate::config::UsageBreakdown::default();
            }
            if opus_due {
                cookie.weekly_opus_usage = crate::config::UsageBreakdown::default();
            }

            // Update/reset boundaries for tracked windows
            if cookie.session_has_reset == Some(true) {
                if let Some(ts) = sess {
                    cookie.session_resets_at = Some(ts);
                } else {
                    // Server indicates no boundary -> stop tracking and clear ts
                    cookie.session_has_reset = Some(false);
                    cookie.session_resets_at = None;
                }
            }
            if cookie.weekly_has_reset == Some(true) {
                if let Some(ts) = week {
                    cookie.weekly_resets_at = Some(ts);
                } else {
                    cookie.weekly_has_reset = Some(false);
                    cookie.weekly_resets_at = None;
                }
            }
            if cookie.weekly_sonnet_has_reset == Some(true) {
                if let Some(ts) = sonnet {
                    cookie.weekly_sonnet_resets_at = Some(ts);
                } else {
                    cookie.weekly_sonnet_has_reset = Some(false);
                    cookie.weekly_sonnet_resets_at = None;
                }
            }
            if cookie.weekly_opus_has_reset == Some(true) {
                if let Some(ts) = opus {
                    cookie.weekly_opus_resets_at = Some(ts);
                } else {
                    cookie.weekly_opus_has_reset = Some(false);
                    cookie.weekly_opus_resets_at = None;
                }
            }
        } else {
            // Network/parse failure: apply fallback only for windows we currently track
            if session_due && session_tracked {
                cookie.session_usage = crate::config::UsageBreakdown::default();
                cookie.session_resets_at = Some(now + SESSION_WINDOW_SECS);
            }
            if weekly_due && weekly_tracked {
                cookie.weekly_usage = crate::config::UsageBreakdown::default();
                cookie.weekly_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
            if sonnet_due && sonnet_tracked {
                cookie.weekly_sonnet_usage = crate::config::UsageBreakdown::default();
                cookie.weekly_sonnet_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
            if opus_due && opus_tracked {
                cookie.weekly_opus_usage = crate::config::UsageBreakdown::default();
                cookie.weekly_opus_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
        }
    }

    async fn fetch_usage_resets(
        cookie: &mut crate::config::CookieStatus,
        handle: &CookieActorHandle,
    ) -> Option<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
        let mut state = ClaudeCodeState::from_cookie(handle.clone(), cookie.clone()).ok()?;
        let usage = state.fetch_usage_metrics().await.ok()?;
        state.return_cookie(None).await;
        if let Some(updated) = state.cookie.clone() {
            *cookie = updated;
        }

        let parse_reset = |obj_key: &str| -> Option<i64> {
            usage
                .get(obj_key)
                .and_then(|o| o.get("resets_at"))
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp())
        };

        Some((
            parse_reset("five_hour"),
            parse_reset("seven_day"),
            parse_reset("seven_day_opus"),
            parse_reset("seven_day_sonnet"),
        ))
    }

    fn local_count_tokens_response(body: &CreateMessageParams) -> axum::response::Response {
        let estimate = CountMessageTokensResponse {
            input_tokens: body.count_tokens(),
        };
        Json(estimate).into_response()
    }

    /// The status upstream actually answered with, and only where the
    /// mapping is exact. ClaudeHttpError carries the code itself, and the
    /// rate-limit cookie reason is built in one place only, the 429 branch
    /// of check_claude. Everything else gets no code rather than a plausible
    /// one, because a status page that guesses is worse than one that admits
    /// it never saw a response.
    fn upstream_code(error: &ClewdrError) -> Option<u16> {
        match error {
            ClewdrError::ClaudeHttpError { code, .. } => Some(code.as_u16()),
            ClewdrError::InvalidCookie {
                reason: crate::config::Reason::TooManyRequest(_),
            }
            | ClewdrError::RateLimited { .. } => Some(429),
            _ => None,
        }
    }

    fn is_context_1m_forbidden(error: &ClewdrError) -> bool {
        if let ClewdrError::ClaudeHttpError { code, inner } = error
            && matches!(code.as_u16(), 400 | 403 | 429)
        {
            let message = inner
                .message
                .as_str()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();

            return message
                .contains("the long context beta is not yet available for this subscription")
                || message.contains(
                    "this authentication style is incompatible with the long context beta header",
                )
                || message.contains("extra usage is required for long context requests");
        }
        false
    }

    /// Whether a failed 1M attempt should be retried without the header. A rate
    /// limit counts: upstream also answers 429 for long context it will not
    /// serve, and benching the whole account over an optional header is the
    /// wrong trade. If the account really is limited the plain retry says so.
    fn is_context_1m_refused(error: &ClewdrError) -> bool {
        Self::is_context_1m_forbidden(error) || matches!(error, ClewdrError::RateLimited { .. })
    }

    fn is_count_tokens_unauthorized(error: &ClewdrError) -> bool {
        if let ClewdrError::ClaudeHttpError { code, .. } = error {
            return match code.as_u16() {
                401 | 404 => true,
                403 => !Self::is_context_1m_forbidden(error),
                _ => false,
            };
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ClaudeErrorBody;

    fn limited(claim: Option<&str>) -> ClewdrError {
        ClewdrError::RateLimited {
            reset_at: 1_800_000_000,
            claim: claim.map(str::to_owned),
            inner: ClaudeErrorBody {
                message: serde_json::json!("slow down"),
                r#type: "rate_limit_error".to_owned(),
                code: Some(429),
            },
        }
    }

    #[test]
    fn the_1m_header_goes_out_only_when_asked_for() {
        assert_eq!(ClaudeCodeState::context_1m_attempts(false), vec![false]);
        assert_eq!(ClaudeCodeState::context_1m_attempts(true), vec![true, false]);
    }

    #[test]
    fn a_rate_limit_on_the_1m_attempt_falls_back_instead_of_benching() {
        assert!(ClaudeCodeState::is_context_1m_refused(&limited(None)));
        let dead = ClewdrError::InvalidCookie {
            reason: Reason::Null,
        };
        assert!(!ClaudeCodeState::is_context_1m_refused(&dead));
    }

    #[test]
    fn only_the_shared_windows_bench_the_account() {
        assert_eq!(
            ClaudeCodeState::account_wide_limit(&limited(Some("five_hour"))),
            Some(1_800_000_000)
        );
        assert_eq!(
            ClaudeCodeState::account_wide_limit(&limited(Some("seven_day"))),
            Some(1_800_000_000)
        );
        // One model's own weekly allowance, extra usage, or a limit upstream
        // did not name: the account keeps serving every other model.
        for claim in [Some("seven_day_opus"), Some("seven_day_oi"), Some("overage"), None] {
            assert_eq!(ClaudeCodeState::account_wide_limit(&limited(claim)), None);
        }
    }

    #[test]
    fn a_passed_on_rate_limit_keeps_its_status_and_its_words() {
        let response = limited(Some("seven_day_oi")).into_response();
        assert_eq!(response.status().as_u16(), 429);
    }

    fn upstream(status: u16) -> ClewdrError {
        ClewdrError::ClaudeHttpError {
            code: wreq::StatusCode::from_u16(status).expect("a status code"),
            inner: ClaudeErrorBody {
                message: serde_json::json!("Overloaded"),
                r#type: "overloaded_error".to_owned(),
                code: Some(status),
            },
        }
    }

    #[test]
    fn only_an_overloaded_upstream_is_waited_out() {
        assert!(ClaudeCodeState::is_overloaded(&upstream(529)));
        // A bad request stays bad however often it is sent, and a server
        // error may have come after the request was already taken in.
        for status in [400, 404, 500, 503] {
            assert!(!ClaudeCodeState::is_overloaded(&upstream(status)));
        }
        assert!(!ClaudeCodeState::is_overloaded(&limited(Some("five_hour"))));
    }

    #[test]
    fn an_overloaded_upstream_reaches_the_client_as_529_when_tries_run_out() {
        assert_eq!(upstream(529).into_response().status().as_u16(), 529);
    }
}
