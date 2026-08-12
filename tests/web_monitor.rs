use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use claude_code_proxy::{monitor::MonitorHandle, registry::Registry, server::app_with_monitor};
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;
use tower::util::ServiceExt;

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("host", "proxy.local:18765")
        .body(Body::empty())
        .expect("request")
}

fn app(monitor: Option<MonitorHandle>) -> axum::Router {
    app_with_monitor(Arc::new(Registry::with_default_alias()), monitor)
}

/// One test function on purpose: `CCP_WEB_MONITOR` is process-wide, so
/// splitting these would race on the environment.
#[tokio::test]
async fn web_monitor_serves_the_terminal_ui_over_http() {
    // Isolate the config dir so a real config.json cannot flip `webMonitor`.
    let config_dir = TempDir::new().expect("temp config dir");
    unsafe {
        std::env::set_var("CCP_CONFIG_DIR", config_dir.path());
        std::env::remove_var("CCP_WEB_MONITOR");
    }

    // Disabled by default: the routes are never registered, so /monitor is just
    // an unknown path.
    let response = app(Some(MonitorHandle::default()))
        .oneshot(get("/monitor"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    unsafe {
        std::env::set_var("CCP_WEB_MONITOR", "1");
    }

    // Enabled but with no monitor store, there is nothing to render.
    let response = app(None).oneshot(get("/monitor")).await.expect("response");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "without a monitor handle the routes must not appear"
    );

    // The page itself.
    let response = app(Some(MonitorHandle::default()))
        .oneshot(get("/monitor"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("page body");
    let page = String::from_utf8_lossy(&bytes);
    assert!(
        page.contains("/monitor/stream"),
        "page must open the stream"
    );
    assert!(
        !page.contains("http://") || !page.contains("<script src"),
        "page must not load external resources"
    );

    // The stream: first frame carries the rendered screen at the requested size.
    let frame = first_frame("/monitor/stream?cols=120&rows=40").await;
    assert_eq!(frame["cols"], 120);
    assert_eq!(frame["rows"], 40);
    let html = frame["html"].as_str().expect("html string");
    assert_eq!(
        html.lines().count(),
        40,
        "one line per terminal row, got: {}",
        html.lines().count()
    );
    for heading in ["Sessions", "Active", "Recent"] {
        assert!(html.contains(heading), "missing {heading} pane");
    }
    // The header shows where the proxy is reachable, taken from the request.
    assert!(
        html.contains("proxy.local:18765"),
        "listen url should come from the Host header"
    );

    // Client-supplied sizes are clamped rather than trusted.
    let frame = first_frame("/monitor/stream?cols=99999&rows=99999").await;
    assert_eq!(frame["cols"], 400);
    assert_eq!(frame["rows"], 200);

    let frame = first_frame("/monitor/stream?cols=1&rows=1").await;
    assert_eq!(frame["cols"], 40);
    assert_eq!(frame["rows"], 10);

    // Omitted sizes fall back to a usable default.
    let frame = first_frame("/monitor/stream").await;
    assert_eq!(frame["cols"], 200);
    assert_eq!(frame["rows"], 50);

    unsafe {
        std::env::remove_var("CCP_WEB_MONITOR");
        std::env::remove_var("CCP_CONFIG_DIR");
    }
}

/// Open the SSE stream and decode its first `frame` event. The stream never
/// ends on its own, so only the first chunk is read.
async fn first_frame(uri: &str) -> Value {
    let response = app(Some(MonitorHandle::default()))
        .oneshot(get(uri))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let mut stream = response.into_body().into_data_stream();
    let chunk = stream
        .next()
        .await
        .expect("a first frame")
        .expect("frame bytes");
    let text = String::from_utf8_lossy(&chunk).to_string();

    assert!(
        text.starts_with("event: frame\n"),
        "expected a frame event, got: {text}"
    );
    let data = text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("data line");
    serde_json::from_str(data).expect("frame json")
}
