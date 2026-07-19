use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    response::{IntoResponse, Response},
    routing::any,
};
use claude_code_proxy::{registry::Registry, server::app};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

/// Mock Anthropic upstream: echoes back what it received so the test can prove
/// the relay changed nothing.
async fn echo(req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();

    if String::from_utf8_lossy(&bytes).contains("\"make_upstream_fail\":true") {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [
                (axum::http::header::CONTENT_TYPE, "application/json"),
                (
                    axum::http::HeaderName::from_static("anthropic-ratelimit-remaining"),
                    "0",
                ),
            ],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"upstream said so"}}"#,
        )
            .into_response();
    }

    if String::from_utf8_lossy(&bytes).contains("\"stream\":true") {
        return (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
            .into_response();
    }

    let headers: serde_json::Map<String, Value> = parts
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                json!(value.to_str().unwrap_or_default()),
            )
        })
        .collect();

    Json(json!({
        "path": parts.uri.path(),
        "query": parts.uri.query(),
        "method": parts.method.as_str(),
        "headers": headers,
        "body": String::from_utf8_lossy(&bytes),
    }))
    .into_response()
}

async fn relay(uri: &str, body: &str) -> Response {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", "Bearer oauth-token-from-claude-code")
        .header("anthropic-beta", "some-beta-flag")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(body.to_string()))
        .expect("request");
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(request)
        .await
        .expect("response")
}

async fn json_body(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

/// One test function on purpose: `CCP_ALIAS_PROVIDER` and `CCP_ANTHROPIC_BASE_URL`
/// are process-wide, so splitting these would race on the environment.
#[tokio::test]
async fn anthropic_passthrough_relays_requests_verbatim() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let upstream_url = format!("http://{}", listener.local_addr().expect("addr"));
    let _upstream = tokio::spawn(async move {
        let _ = axum::serve(listener, Router::new().fallback(any(echo))).await;
    });

    unsafe {
        std::env::set_var("CCP_ALIAS_PROVIDER", "anthropic");
        std::env::set_var("CCP_ANTHROPIC_BASE_URL", &upstream_url);
    }

    // Body is relayed byte for byte, including the `[1m]` model suffix that the
    // translated path would otherwise strip, and fields this proxy has no
    // schema for.
    let raw = r#"{"model":"claude-sonnet-5[1m]","some_future_field":{"nested":1},"messages":[]}"#;
    let echoed = json_body(relay("/v1/messages?beta=true", raw).await).await;
    assert_eq!(echoed["body"].as_str().expect("body"), raw);
    assert_eq!(echoed["path"], "/v1/messages");
    assert_eq!(echoed["query"], "beta=true");

    // Client credentials and Anthropic headers reach the upstream untouched.
    assert_eq!(
        echoed["headers"]["authorization"],
        "Bearer oauth-token-from-claude-code"
    );
    assert_eq!(echoed["headers"]["anthropic-beta"], "some-beta-flag");
    assert_eq!(echoed["headers"]["anthropic-version"], "2023-06-01");

    // count_tokens goes upstream rather than to the local tokenizer.
    let echoed = json_body(
        relay(
            "/v1/messages/count_tokens",
            r#"{"model":"claude-sonnet-5","messages":[]}"#,
        )
        .await,
    )
    .await;
    assert_eq!(echoed["path"], "/v1/messages/count_tokens");

    // Routes this proxy does not implement fall through to Anthropic.
    let echoed = json_body(relay("/v1/organizations/whoami", "{}").await).await;
    assert_eq!(echoed["path"], "/v1/organizations/whoami");
    assert_eq!(
        echoed["headers"]["authorization"],
        "Bearer oauth-token-from-claude-code"
    );

    // Streaming responses are relayed as SSE, not buffered into JSON.
    let response = relay(
        "/v1/messages",
        r#"{"model":"claude-sonnet-5","stream":true,"messages":[]}"#,
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("sse body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("event: message_start"), "got: {text}");
    assert!(text.contains("event: message_stop"), "got: {text}");

    // Upstream errors are relayed verbatim: status, body and Anthropic's own
    // response headers survive, rather than being rewritten into a proxy error.
    let response = relay(
        "/v1/messages",
        r#"{"model":"claude-sonnet-5","make_upstream_fail":true}"#,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-remaining")
            .and_then(|value| value.to_str().ok()),
        Some("0")
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("error body");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"upstream said so"}}"#
    );

    // Small/fast model IDs pass through with ANTHROPIC_SMALL_FAST_MODEL unset.
    assert!(std::env::var("ANTHROPIC_SMALL_FAST_MODEL").is_err());
    for model in ["haiku", "claude-haiku-4-5", "claude-haiku-4-5-20251001"] {
        let echoed =
            json_body(relay("/v1/messages", &format!(r#"{{"model":"{model}"}}"#)).await).await;
        assert_eq!(
            echoed["path"], "/v1/messages",
            "{model} should pass through"
        );
    }

    // Models owned by other providers keep their translated route.
    let response = relay("/v1/messages", r#"{"model":"no-such-model"}"#).await;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);

    unsafe {
        std::env::remove_var("CCP_ALIAS_PROVIDER");
        std::env::remove_var("CCP_ANTHROPIC_BASE_URL");
    }
}
