use std::sync::Arc;

use axum::{Extension, extract::State, response::Response};

use crate::{
    error::ClewdrError,
    middleware::claude::{ClaudeContext, ClaudeWebPreprocess},
    providers::{
        LLMProvider,
        claude::{ClaudeInvocation, ClaudeProviderResponse, ClaudeWebProvider},
    },
    services::busy::{self, Busy},
};
/// Axum handler for the API messages
/// Main API endpoint for handling message requests to Claude
/// Processes messages, handles retries, and returns responses in stream or non-stream mode
///
/// # Arguments
/// * `XApiKey(_)` - API key authentication
/// * `state` - Application state containing client information
/// * `p` - Request body containing messages and configuration
///
/// # Returns
/// * `Response` - Stream or JSON response from Claude
pub async fn api_claude_web(
    State(provider): State<Arc<ClaudeWebProvider>>,
    ClaudeWebPreprocess(params, context): ClaudeWebPreprocess,
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
