//! HTTP client for an OpenAI-compatible chat-completions endpoint.
//!
//! Deliberately thinner than the Kimi client: authentication is a static API
//! key from the environment, so there is no token store, no refresh, and
//! nothing to persist.

use crate::providers::kimi::translate::request::KimiChatRequest;
use crate::retry::{MAX_RATE_LIMIT_RETRIES, compute_backoff_delay};
use once_cell::sync::Lazy;
use std::time::Duration;

#[derive(Debug)]
pub struct OpenCodeError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
}

pub struct OpenCodeResponse {
    pub body: Vec<u8>,
}

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// Post a chat-completions request, retrying on rate limits the same way the
/// Kimi provider does.
pub async fn post_chat_completions(
    body: &KimiChatRequest,
) -> Result<OpenCodeResponse, OpenCodeError> {
    let Some(api_key) = crate::config::opencode_api_key() else {
        return Err(OpenCodeError {
            status: 401,
            message: "Not authenticated".to_string(),
            detail: Some(
                "Set OPENCODE_API_KEY (or CCP_OPENCODE_API_KEY) to use the opencode provider"
                    .to_string(),
            ),
        });
    };

    let url = format!(
        "{}/chat/completions",
        crate::config::opencode_base_url().trim_end_matches('/')
    );
    let payload = serde_json::to_vec(body).map_err(|err| OpenCodeError {
        status: 500,
        message: "Failed to serialize request".to_string(),
        detail: Some(err.to_string()),
    })?;

    let mut attempt = 0u32;
    loop {
        let response = CLIENT
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {api_key}"))
            .body(payload.clone())
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(err) => {
                return Err(OpenCodeError {
                    status: 0,
                    message: "Network error".to_string(),
                    detail: Some(err.to_string()),
                });
            }
        };

        let status = response.status().as_u16();
        if status == 429 && attempt < MAX_RATE_LIMIT_RETRIES {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(std::string::ToString::to_string);
            let delay = compute_backoff_delay(attempt, retry_after.as_deref());
            tokio::time::sleep(Duration::from_millis(delay.wait_ms)).await;
            attempt += 1;
            continue;
        }

        let bytes = response.bytes().await.map_err(|err| OpenCodeError {
            status,
            message: "Failed to read upstream response".to_string(),
            detail: Some(err.to_string()),
        })?;

        if status >= 400 {
            return Err(OpenCodeError {
                status,
                message: format!("Upstream returned HTTP {status}"),
                detail: Some(String::from_utf8_lossy(&bytes).to_string()),
            });
        }

        return Ok(OpenCodeResponse {
            body: bytes.to_vec(),
        });
    }
}
