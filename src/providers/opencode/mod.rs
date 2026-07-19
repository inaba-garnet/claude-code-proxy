//! Provider for an arbitrary OpenAI-compatible endpoint, defaulting to OpenCode Go.
//!
//! The Anthropic <-> chat-completions translation is entirely the Kimi
//! provider's: both speak the same wire format, including the `reasoning_content`
//! deltas that DeepSeek emits, which Kimi's reducer already maps to Anthropic
//! thinking blocks. Only transport, auth and model naming differ.

pub mod client;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::kimi::count_tokens;
use crate::providers::kimi::translate::accumulate::accumulate_response;
use crate::providers::kimi::translate::request::{TranslateOptions, translate_request};
use crate::providers::kimi::translate::stream::translate_stream_bytes;

pub struct OpenCodeProvider;

impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeProvider {
    pub fn new() -> Self {
        Self
    }
}

/// The model to send upstream. The `opencode-go/` routing prefix is stripped,
/// Anthropic-style aliases resolve to the first configured model so that
/// `CCP_ALIAS_PROVIDER=opencode` works, and anything else is passed through
/// untouched, since only the endpoint knows its own catalog.
fn resolve_model(model: &str) -> String {
    let model = model
        .strip_prefix(crate::registry::OPENCODE_PREFIX)
        .unwrap_or(model);
    if crate::registry::is_anthropic_alias(model) {
        return crate::config::opencode_models()
            .first()
            .cloned()
            .unwrap_or_else(|| model.to_string());
    }
    model.to_string()
}

#[async_trait]
impl Provider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn supported_models(&self) -> Vec<String> {
        crate::config::opencode_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &OPENCODE_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let requested = body.model.clone().unwrap_or_default();
        let resolved = resolve_model(&requested);

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }

        let mut translated = match translate_request(
            &body,
            TranslateOptions {
                session_id: ctx.session_id.clone(),
            },
        ) {
            Ok(translated) => translated,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    err.to_string(),
                );
            }
        };
        // `translate_request` names a Kimi model; this endpoint has its own.
        translated.model = resolved.clone();
        // Kimi's `thinking` opt-in is not universal: minimax-m3 rejects the
        // whole request over it, and the models that do reason emit
        // `reasoning_content` without being asked. Dropping it costs nothing.
        translated.thinking = None;

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        if let Some(capture) = ctx.traffic.as_ref() {
            capture.write_json(
                "020-opencode-request",
                &serde_json::to_value(&translated).unwrap_or_default(),
            );
        }

        let upstream = match client::post_chat_completions(&translated).await {
            Ok(upstream) => upstream,
            Err(err) => return map_error_to_response(&err),
        };

        if want_stream {
            let sse_bytes = match translate_stream_bytes(&upstream.body, &message_id, &requested) {
                Ok(bytes) => bytes,
                Err(err) => {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        format!("Stream translation error: {err}"),
                    );
                }
            };
            if let Some(monitor) = ctx.monitor.as_ref() {
                let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse_bytes);
                monitor.stream_progress(
                    &ctx.req_id,
                    sse_bytes.len() as u64,
                    count_sse_events(&sse_bytes),
                    input_tokens,
                    output_tokens,
                );
            }
            let headers = [
                (http::header::CONTENT_TYPE, "text/event-stream"),
                (http::header::CACHE_CONTROL, "no-cache"),
                (http::header::CONNECTION, "keep-alive"),
            ];
            (headers, sse_bytes).into_response()
        } else {
            match accumulate_response(&upstream.body, &message_id, &requested) {
                Ok(json) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()),
                            json.pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(err) => json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Accumulation error: {err}"),
                ),
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let tokens = count_tokens::count_tokens(&body);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

fn map_error_to_response(err: &client::OpenCodeError) -> Response {
    let detail = err.detail.as_deref().unwrap_or(err.message.as_str());
    match err.status {
        401 | 403 => json_error(StatusCode::UNAUTHORIZED, "authentication_error", detail),
        429 => json_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", detail),
        status if (400..500).contains(&status) => {
            json_error(StatusCode::BAD_REQUEST, "invalid_request_error", detail)
        }
        _ => json_error(StatusCode::BAD_GATEWAY, "api_error", detail),
    }
}

/// No login flow: the key lives in the environment or config file.
#[derive(Clone, Copy)]
struct OpenCodeCli;

impl CliHandlers for OpenCodeCli {
    fn login(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "opencode: set OPENCODE_API_KEY (or CCP_OPENCODE_API_KEY); there is no login flow"
        ))
    }

    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }

    fn status(&self) -> anyhow::Result<()> {
        if crate::config::opencode_api_key().is_some() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "opencode: the proxy stores no credentials; unset OPENCODE_API_KEY instead"
        ))
    }
}

const OPENCODE_CLI: OpenCodeCli = OpenCodeCli;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_aliases_resolve_to_the_first_configured_model() {
        assert_eq!(
            resolve_model("sonnet"),
            crate::registry::OPENCODE_DEFAULT_MODELS[0]
        );
    }

    #[test]
    fn explicit_model_ids_pass_through_untouched() {
        assert_eq!(resolve_model("deepseek-v4-flash"), "deepseek-v4-flash");
        assert_eq!(resolve_model("some-other-model"), "some-other-model");
    }

    /// The routing prefix must not reach the upstream, which knows only its
    /// own bare ids.
    #[test]
    fn routing_prefix_is_stripped_before_the_upstream_sees_it() {
        assert_eq!(resolve_model("opencode-go/kimi-k2.6"), "kimi-k2.6");
        assert_eq!(resolve_model("opencode-go/grok-4.5"), "grok-4.5");
    }
}
