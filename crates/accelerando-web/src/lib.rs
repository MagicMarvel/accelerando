//! Self-contained web UI helpers for embedding a read-only backtest result viewer.
//!
//! No node, no build step: pass a [`BacktestResult`] to [`serve`] and open the printed localhost URL.

use std::io::Cursor;

use accelerando_core::BacktestResult;
use tiny_http::{Header, Response, Server};

const STUDIO_HTML: &str = include_str!("studio.html");

/// The embedded result/studio page HTML.
///
/// Applications can reuse this if they want to provide their own JSON API around the page.
pub fn studio_html() -> &'static str {
    STUDIO_HTML
}

/// Start the read-only result server and block, serving `result` on `port`.
pub fn serve(result: &BacktestResult, port: u16) -> std::io::Result<()> {
    let json = serde_json::to_string(result).expect("serialize result");
    let addr = format!("0.0.0.0:{port}");
    let server = Server::http(&addr)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("bind {addr}: {e}")))?;
    println!("Accelerando result viewer → http://localhost:{port}  (Ctrl+C to stop)");

    for request in server.incoming_requests() {
        let url = request.url().split('?').next().unwrap_or("/");
        let response = match url {
            "/api/result" | "/result.json" => json_response(&json),
            "/api/progress" => json_response(r#"{"running":false,"has_result":true}"#),
            "/" | "/index.html" => html_response(STUDIO_HTML),
            _ => Response::from_string("not found").with_status_code(404),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

fn html_response(body: &str) -> Response<Cursor<Vec<u8>>> {
    let mut r = Response::from_string(body);
    r.add_header(content_type("text/html; charset=utf-8"));
    r
}

fn json_response(body: &str) -> Response<Cursor<Vec<u8>>> {
    let mut r = Response::from_string(body.to_string());
    r.add_header(content_type("application/json; charset=utf-8"));
    r
}

fn content_type(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).unwrap()
}
