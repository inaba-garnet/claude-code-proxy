//! Verbatim forwarding to the Anthropic API.
//!
//! Nothing here translates. The request body bytes, query string, and client
//! headers are relayed unchanged so that Claude Code's own credentials
//! (subscription OAuth or `x-api-key`) and any request fields this proxy does
//! not know about keep working across Claude Code updates. Only hop-by-hop
//! headers are dropped.

use crate::anthropic::json_error;
use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::Response,
};
use bytes::Bytes;
use futures_util::TryStreamExt;
use once_cell::sync::Lazy;
use std::time::Duration;

/// Headers scoped to a single transport hop; relaying them corrupts the
/// connection or the framing that `reqwest`/`axum` recompute themselves.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// No overall timeout: streamed completions can outlive any fixed budget, and
/// cutting one short would be a behaviour difference from stock Claude Code.
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name)
}

/// Join the configured base URL with the incoming path and query, preserving
/// the query string verbatim (including flags such as `?beta=true`).
fn upstream_url(uri: &Uri) -> String {
    let base = crate::config::anthropic_base_url();
    let base = base.trim_end_matches('/');
    match uri.query() {
        Some(query) => format!("{base}{}?{query}", uri.path()),
        None => format!("{base}{}", uri.path()),
    }
}

/// Relay a request to the Anthropic API and stream the response back as-is.
pub async fn forward(method: Method, uri: &Uri, headers: &HeaderMap, body: Bytes) -> Response {
    let url = upstream_url(uri);
    let mut request = CLIENT.request(method, &url);
    for (name, value) in headers {
        if !is_hop_by_hop(name.as_str()) {
            request = request.header(name, value);
        }
    }

    let upstream = match request.body(body).send().await {
        Ok(response) => response,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Upstream request to {url} failed: {err}"),
            );
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if !is_hop_by_hop(name.as_str()) {
            response = response.header(name, value);
        }
    }

    // Stream rather than buffer so SSE reaches the client token by token.
    let stream = upstream
        .bytes_stream()
        .map_err(|err| std::io::Error::other(err.to_string()));
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Failed to relay upstream response: {err}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_path_and_query() {
        let uri: Uri = "/v1/messages?beta=true".parse().expect("uri");
        assert_eq!(
            upstream_url(&uri),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
    }

    #[test]
    fn omits_question_mark_without_query() {
        let uri: Uri = "/v1/messages/count_tokens".parse().expect("uri");
        assert_eq!(
            upstream_url(&uri),
            "https://api.anthropic.com/v1/messages/count_tokens"
        );
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        assert!(is_hop_by_hop("host"));
        assert!(is_hop_by_hop("content-length"));
        assert!(!is_hop_by_hop("authorization"));
        assert!(!is_hop_by_hop("anthropic-beta"));
        assert!(!is_hop_by_hop("x-api-key"));
    }
}
