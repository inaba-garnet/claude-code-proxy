//! Browser view of the monitor TUI.
//!
//! Containers have no TTY, so the terminal dashboard never appears. Rather than
//! building a second dashboard that would drift from the first, this renders the
//! *same* `tui::render` call into an off-screen buffer and converts the cells to
//! HTML, streamed to the browser over SSE. Whatever the TUI grows, this shows.
//!
//! The view is read-only: no keys are forwarded, because one shared selection
//! cursor across arbitrarily many viewers is worse than none.

use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use serde::Deserialize;
use std::time::Duration;

use crate::monitor::MonitorHandle;

/// Matches the TUI's own redraw cadence (`event::poll` timeout in `tui.rs`).
const FRAME_INTERVAL: Duration = Duration::from_millis(250);

/// Emitted when the screen has not changed, so idle viewers cost almost nothing
/// but connections still survive intermediaries that drop silent streams.
const HEARTBEAT_AFTER: Duration = Duration::from_secs(15);

/// Bounds on the requested grid. A browser reports its real size, but the query
/// string is client-controlled and each cell costs memory in the render buffer.
const MIN_COLS: u16 = 40;
const MAX_COLS: u16 = 400;
const MIN_ROWS: u16 = 10;
const MAX_ROWS: u16 = 200;
const DEFAULT_COLS: u16 = 200;
const DEFAULT_ROWS: u16 = 50;

/// Page defaults, mirroring `BG` and `WHITE` in `tui.rs`. Cells that carry
/// `Color::Reset` inherit these instead of the browser's own colors.
const DEFAULT_FG: (u8, u8, u8) = (240, 244, 248);
const DEFAULT_BG: (u8, u8, u8) = (18, 18, 22);

/// Routes for the browser monitor, self-contained so `server.rs` only has to
/// merge them. `with_state` fixes the handlers' own state and lets the result
/// adopt whatever state the outer router carries, so this composes without the
/// proxy's `AppState` being involved at all.
pub fn router<S>(monitor: MonitorHandle) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/monitor", get(page))
        .route("/monitor/stream", get(stream))
        .with_state(monitor)
}

async fn page() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("web_monitor.html"),
    )
        .into_response()
}

/// Sizes arrive as `u32` so that an oversized request is clamped rather than
/// rejected: a browser reporting an absurd grid should still get a frame.
#[derive(Debug, Deserialize)]
struct FrameQuery {
    cols: Option<u32>,
    rows: Option<u32>,
}

fn clamp_dimension(value: Option<u32>, default: u16, min: u16, max: u16) -> u16 {
    value
        .map(|value| value.min(u32::from(u16::MAX)) as u16)
        .unwrap_or(default)
        .clamp(min, max)
}

async fn stream(
    State(monitor): State<MonitorHandle>,
    headers: HeaderMap,
    Query(query): Query<FrameQuery>,
) -> Response {
    let cols = clamp_dimension(query.cols, DEFAULT_COLS, MIN_COLS, MAX_COLS);
    let rows = clamp_dimension(query.rows, DEFAULT_ROWS, MIN_ROWS, MAX_ROWS);
    let listen_url = listen_url_from_host(&headers);

    let state = FrameStream {
        monitor,
        listen_url,
        cols,
        rows,
        tick: 0,
        last_html: None,
        quiet_for: Duration::ZERO,
        first: true,
    };

    // Loops rather than yielding an empty chunk: a zero-length body chunk ends
    // a chunked response, which would close the stream instead of idling.
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if state.first {
                state.first = false;
            } else {
                tokio::time::sleep(FRAME_INTERVAL).await;
            }
            if let Some(chunk) = state.next_chunk() {
                return Some((Ok::<Bytes, std::io::Error>(chunk), state));
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        // Reverse proxies that buffer by default would hold frames back.
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to open monitor stream: {err}"),
            )
                .into_response()
        })
}

struct FrameStream {
    monitor: MonitorHandle,
    listen_url: String,
    cols: u16,
    rows: u16,
    tick: usize,
    last_html: Option<String>,
    quiet_for: Duration,
    first: bool,
}

impl FrameStream {
    /// One SSE chunk, or `None` when the screen has not changed — an idle
    /// dashboard then costs nothing but a periodic keepalive comment.
    fn next_chunk(&mut self) -> Option<Bytes> {
        self.tick = self.tick.wrapping_add(1);
        let snapshot = self.monitor.snapshot();
        let buffer = crate::tui::render_offscreen(
            &snapshot,
            &self.listen_url,
            self.tick,
            self.cols,
            self.rows,
        );
        let html = buffer_to_html(&buffer);

        if self.last_html.as_deref() == Some(html.as_str()) {
            self.quiet_for += FRAME_INTERVAL;
            if self.quiet_for < HEARTBEAT_AFTER {
                return None;
            }
            self.quiet_for = Duration::ZERO;
            return Some(Bytes::from_static(b": keepalive\n\n"));
        }

        self.quiet_for = Duration::ZERO;
        let payload = serde_json::json!({
            "html": &html,
            "cols": self.cols,
            "rows": self.rows,
        });
        self.last_html = Some(html);
        // `to_string` never fails for a value built from strings and integers.
        let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        Some(Bytes::from(crate::anthropic::sse::encode_sse_event(
            Some("frame"),
            &data,
        )))
    }
}

/// The TUI header shows the address the proxy is reachable at. In a browser the
/// only thing that knows it is the request itself.
fn listen_url_from_host(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|host| !host.is_empty())
        .map(|host| format!("http://{host}"))
        .unwrap_or_else(|| "http://localhost".to_string())
}

/// Convert a rendered terminal buffer to HTML, collapsing runs of equally
/// styled cells into one `<span>`.
fn buffer_to_html(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::with_capacity((area.width as usize * area.height as usize) * 2);

    for y in 0..area.height {
        let mut run = String::new();
        let mut run_style: Option<CellStyle> = None;

        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            let style = CellStyle::of(cell.fg, cell.bg, cell.modifier);
            if run_style.as_ref() != Some(&style) {
                if let Some(previous) = run_style.take() {
                    previous.write_span(&mut out, &run);
                }
                run.clear();
                run_style = Some(style);
            }
            push_escaped(&mut run, cell.symbol());
        }
        if let Some(previous) = run_style.take() {
            previous.write_span(&mut out, &run);
        }
        if y + 1 < area.height {
            out.push('\n');
        }
    }
    out
}

fn push_escaped(out: &mut String, symbol: &str) {
    for character in symbol.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CellStyle {
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    bold: bool,
    dim: bool,
    italic: bool,
    underlined: bool,
}

impl CellStyle {
    fn of(fg: Color, bg: Color, modifier: Modifier) -> Self {
        let mut foreground = rgb(fg, DEFAULT_FG);
        let mut background = rgb(bg, DEFAULT_BG);
        if modifier.contains(Modifier::REVERSED) {
            std::mem::swap(&mut foreground, &mut background);
        }
        Self {
            fg: foreground,
            bg: background,
            bold: modifier.contains(Modifier::BOLD),
            dim: modifier.contains(Modifier::DIM),
            italic: modifier.contains(Modifier::ITALIC),
            underlined: modifier.contains(Modifier::UNDERLINED),
        }
    }

    fn is_plain(&self) -> bool {
        self.fg == DEFAULT_FG
            && self.bg == DEFAULT_BG
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underlined
    }

    fn write_span(&self, out: &mut String, text: &str) {
        if text.is_empty() {
            return;
        }
        // Cells that already look like the page save a wrapper each.
        if self.is_plain() {
            out.push_str(text);
            return;
        }
        out.push_str("<span style=\"color:");
        out.push_str(&hex(self.fg));
        out.push_str(";background:");
        out.push_str(&hex(self.bg));
        if self.bold {
            out.push_str(";font-weight:700");
        }
        if self.dim {
            out.push_str(";opacity:.6");
        }
        if self.italic {
            out.push_str(";font-style:italic");
        }
        if self.underlined {
            out.push_str(";text-decoration:underline");
        }
        out.push_str("\">");
        out.push_str(text);
        out.push_str("</span>");
    }
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Resolve a ratatui color to RGB. The monitor palette is entirely `Color::Rgb`
/// (see `tui.rs`), so the remaining arms only matter if a widget default leaks
/// through.
fn rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Reset => fallback,
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (170, 0, 0),
        Color::Green => (0, 170, 0),
        Color::Yellow => (170, 85, 0),
        Color::Blue => (0, 0, 170),
        Color::Magenta => (170, 0, 170),
        Color::Cyan => (0, 170, 170),
        Color::Gray => (170, 170, 170),
        Color::DarkGray => (85, 85, 85),
        Color::LightRed => (255, 85, 85),
        Color::LightGreen => (85, 255, 85),
        Color::LightYellow => (255, 255, 85),
        Color::LightBlue => (85, 85, 255),
        Color::LightMagenta => (255, 85, 255),
        Color::LightCyan => (85, 255, 255),
        Color::White => (255, 255, 255),
        Color::Indexed(index) => indexed(index),
    }
}

/// The xterm 256-color table: 16 base colors, a 6×6×6 cube, then 24 greys.
fn indexed(index: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (170, 0, 0),
        (0, 170, 0),
        (170, 85, 0),
        (0, 0, 170),
        (170, 0, 170),
        (0, 170, 170),
        (170, 170, 170),
        (85, 85, 85),
        (255, 85, 85),
        (85, 255, 85),
        (255, 255, 85),
        (85, 85, 255),
        (255, 85, 255),
        (85, 255, 255),
        (255, 255, 255),
    ];
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match index {
        0..=15 => BASE[index as usize],
        16..=231 => {
            let value = index - 16;
            (
                CUBE[(value / 36) as usize],
                CUBE[((value % 36) / 6) as usize],
                CUBE[(value % 6) as usize],
            )
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::mock_state;
    use ratatui::layout::Rect;

    fn buffer_with(text: &str) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, text.chars().count() as u16, 1));
        for (x, character) in text.chars().enumerate() {
            buffer[(x as u16, 0)].set_symbol(&character.to_string());
        }
        buffer
    }

    #[test]
    fn escapes_html_metacharacters() {
        let html = buffer_to_html(&buffer_with("a<b&c>d"));
        assert!(html.contains("a&lt;b&amp;c&gt;d"), "got: {html}");
        assert!(!html.contains("<b"), "raw tag leaked: {html}");
    }

    #[test]
    fn rgb_colors_become_css_hex() {
        // TEAL in tui.rs.
        assert_eq!(hex(rgb(Color::Rgb(78, 201, 176), DEFAULT_FG)), "#4ec9b0");
    }

    #[test]
    fn reset_falls_back_to_the_page_colors() {
        let style = CellStyle::of(Color::Reset, Color::Reset, Modifier::empty());
        assert_eq!(style.fg, DEFAULT_FG);
        assert_eq!(style.bg, DEFAULT_BG);
        assert!(style.is_plain(), "unstyled cells should need no span");
    }

    #[test]
    fn reversed_swaps_foreground_and_background() {
        let style = CellStyle::of(Color::Rgb(1, 2, 3), Color::Rgb(4, 5, 6), Modifier::REVERSED);
        assert_eq!(style.fg, (4, 5, 6));
        assert_eq!(style.bg, (1, 2, 3));
    }

    #[test]
    fn indexed_covers_base_cube_and_greyscale() {
        assert_eq!(indexed(1), (170, 0, 0));
        assert_eq!(indexed(196), (255, 0, 0));
        assert_eq!(indexed(231), (255, 255, 255));
        assert_eq!(indexed(232), (8, 8, 8));
        assert_eq!(indexed(255), (238, 238, 238));
    }

    /// Equally styled neighbours must share one span, or a full frame would be
    /// one wrapper per character.
    #[test]
    fn runs_of_one_style_collapse_into_a_single_span() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        for x in 0..4u16 {
            buffer[(x, 0)].set_symbol("x");
            buffer[(x, 0)].fg = Color::Rgb(10, 20, 30);
        }
        let html = buffer_to_html(&buffer);
        assert_eq!(html.matches("<span").count(), 1, "got: {html}");
        assert!(html.contains(">xxxx</span>"), "got: {html}");
    }

    #[test]
    fn rows_are_newline_separated_without_a_trailing_newline() {
        let buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let html = buffer_to_html(&buffer);
        assert_eq!(html, "   \n   ");
    }

    /// The whole point of the module: the browser frame is the TUI's own render.
    #[test]
    fn renders_the_real_monitor_panes() {
        let buffer =
            crate::tui::render_offscreen(&mock_state(), "http://localhost:18765", 0, 160, 48);
        let html = buffer_to_html(&buffer);
        for heading in ["Sessions", "Active", "Recent"] {
            assert!(html.contains(heading), "missing {heading} pane in: {html}");
        }
        assert!(html.contains("localhost:18765"), "missing listen url");
    }
}
