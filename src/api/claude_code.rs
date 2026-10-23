use std::sync::Arc;

use axum::{Extension, extract::State, response::Response};

use crate::{
    error::ClewdrError,
    middleware::claude::{ClaudeCodePreprocess, ClaudeContext},
    providers::{
        LLMProvider,
        claude::{ClaudeCodeProvider, ClaudeInvocation, ClaudeProviderResponse},
    },
    services::busy::{self, Busy},
};

pub async fn api_claude_code(
    State(provider): State<Arc<ClaudeCodeProvider>>,
    ClaudeCodePreprocess(params, context): ClaudeCodePreprocess,
) -> Result<(Extension<ClaudeContext>, Response), ClewdrError> {
    // 先计数再看闸门：正在开始的更新要么看到这条请求而放弃，要么由闸门把它拒收
    let _busy = Busy::new();
    if busy::paused() {
        return Err(ClewdrError::Updating);
    }
    let ClaudeProviderResponse { context, response } = provider
        .invoke(ClaudeInvocation::messages(params, context.clone()))
        .await?;
    Ok((Extension(context), response))
}

pub async fn api_claude_code_count_tokens(
    State(provider): State<Arc<ClaudeCodeProvider>>,
    ClaudeCodePreprocess(mut params, context): ClaudeCodePreprocess,
) -> Result<Response, ClewdrError> {
    params.stream = Some(false);
    let ClaudeProviderResponse { response, .. } = provider
        .invoke(ClaudeInvocation::count_tokens(params, context))
        .await?;
    Ok(response)
}
