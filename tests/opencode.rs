use axum::{
    Router,
    body::Body,
    extract::Request,
    response::{IntoResponse, Response},
    routing::any,
};
use claude_code_proxy::{registry::Registry, server::app};
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};
use tower::ServiceExt;

static OBSERVED: OnceLock<Arc<Mutex<Option<Value>>>> = OnceLock::new();

fn observed() -> Arc<Mutex<Option<Value>>> {
    OBSERVED.get_or_init(|| Arc::new(Mutex::new(None))).clone()
}

/// Mock OpenAI-compatible upstream. Records the translated request, then replies
/// with a chat-completions SSE stream shaped like DeepSeek's, including the
/// `reasoning_content` deltas.
async fn upstream(req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();

    let mut record = serde_json::json!({
        "path": parts.uri.path(),
        "authorization": parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    });
    if let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) {
        record["body"] = parsed;
    }
    *observed().lock().expect("lock") = Some(record);

    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"weighing it up\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello from deepseek\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        ),
    )
        .into_response()
}

async fn send(model: &str) -> Response {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"model":"{model}","max_tokens":64,"messages":[{{"role":"user","content":"hi"}}]}}"#
        )))
        .expect("request");
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(request)
        .await
        .expect("response")
}

/// One test function: the environment is process-wide, so splitting would race.
#[tokio::test]
async fn opencode_translates_through_the_kimi_wire_format() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let _server = tokio::spawn(async move {
        let _ = axum::serve(listener, Router::new().fallback(any(upstream))).await;
    });

    unsafe {
        std::env::set_var("CCP_OPENCODE_BASE_URL", &base_url);
        std::env::set_var("CCP_OPENCODE_API_KEY", "test-key");
    }

    let response = send("deepseek-v4-flash").await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let anthropic: Value = serde_json::from_slice(&bytes).expect("anthropic json");

    // The upstream saw a chat-completions request carrying the configured model
    // and the API key, at the OpenAI-compatible path.
    let record = observed().lock().expect("lock").clone().expect("observed");
    assert_eq!(record["path"], "/chat/completions");
    assert_eq!(record["authorization"], "Bearer test-key");
    assert_eq!(record["body"]["model"], "deepseek-v4-flash");
    assert_eq!(record["body"]["messages"][0]["role"], "user");

    // DeepSeek's reasoning_content becomes an Anthropic thinking block, and the
    // reply text and usage survive the round trip.
    let content = anthropic["content"].as_array().expect("content array");
    assert!(
        content.iter().any(|block| block["type"] == "thinking"
            && block["thinking"].as_str().unwrap_or_default() == "weighing it up"),
        "expected a thinking block, got: {content:?}"
    );
    assert!(
        content.iter().any(|block| block["type"] == "text"
            && block["text"].as_str().unwrap_or_default() == "hello from deepseek"),
        "expected the reply text, got: {content:?}"
    );
    assert_eq!(anthropic["usage"]["input_tokens"], 11);
    assert_eq!(anthropic["usage"]["output_tokens"], 7);

    // A missing key is reported as an auth error rather than a hang or a 500.
    unsafe {
        std::env::remove_var("CCP_OPENCODE_API_KEY");
        std::env::remove_var("OPENCODE_API_KEY");
    }
    let response = send("deepseek-v4-flash").await;
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);

    unsafe {
        std::env::remove_var("CCP_OPENCODE_BASE_URL");
    }
}
