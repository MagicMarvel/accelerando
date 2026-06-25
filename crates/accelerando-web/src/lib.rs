//! A self-contained dashboard server. It serializes a [`BacktestResult`] to JSON once and serves
//! an embedded HTML/Canvas page that renders the footprint chart (with regime bands + trade
//! markers), the equity curve, drawdown, and a metrics table. No node, no build step.

use std::io::Cursor;

use accelerando_core::BacktestResult;
use tiny_http::{Header, Response, Server};

const INDEX_HTML: &str = include_str!("dashboard.html");
const STUDIO_HTML: &str = include_str!("studio.html");

/// The interactive studio page (config form + progress + saved runs + chart). Served by the CLI's
/// `studio` command, which provides the JSON API the page talks to.
pub fn studio_html() -> &'static str {
    STUDIO_HTML
}

/// Start the dashboard server and block, serving `result` on `port`.
pub fn serve(result: &BacktestResult, port: u16) -> std::io::Result<()> {
    let json = serde_json::to_string(result).expect("serialize result");
    let addr = format!("0.0.0.0:{port}");
    let server = Server::http(&addr).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("bind {addr}: {e}"))
    })?;
    println!("Accelerando dashboard → http://localhost:{port}  (Ctrl+C to stop)");

    for request in server.incoming_requests() {
        let url = request.url().split('?').next().unwrap_or("/");
        let response = match url {
            "/result.json" => json_response(&json),
            "/" | "/index.html" => html_response(INDEX_HTML),
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
