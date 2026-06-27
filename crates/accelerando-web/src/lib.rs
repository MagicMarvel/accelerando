//! Self-contained web UI helpers for embedding a read-only backtest result viewer.
//!
//! No node, no build step: pass a [`BacktestResult`] to [`serve`] and open the printed localhost URL.

use std::io::Cursor;

use accelerando_core::{BacktestResult, ExperimentResult, ExperimentRunSummary};
use serde::Serialize;
use tiny_http::{Header, Response, Server};

const STUDIO_HTML: &str = include_str!("studio.html");
const EXPERIMENT_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Accelerando — experiment</title>
<style>
:root{font-family:Inter,"Segoe UI",Arial,sans-serif;background:#f6f7f9;color:#111827}
body{margin:0}
header{padding:10px 16px;background:#0f172a;color:white;display:flex;gap:12px;align-items:baseline}
.title{font-size:16px;font-weight:800;letter-spacing:.3px}.tag{font-size:12px;color:#94a3b8}
main{padding:14px}
.cards{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:12px}
.card{min-width:120px;padding:8px 10px;border:1px solid #e5e7eb;border-radius:8px;background:#fff}
.k{font-size:10px;color:#6b7280;text-transform:uppercase;letter-spacing:.4px}.v{font-size:18px;font-weight:800;margin-top:2px}
table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #e5e7eb;border-radius:8px;overflow:hidden}
th,td{padding:8px 10px;border-bottom:1px solid #eef2f7;text-align:right;font-size:13px;white-space:nowrap}
th:first-child,td:first-child,th:nth-child(2),td:nth-child(2){text-align:left}
th{background:#f8fafc;color:#475569;font-size:11px;text-transform:uppercase;letter-spacing:.4px}
tr{cursor:pointer}tr:hover{background:#eff6ff}.pos{color:#2563eb}.neg{color:#d97706}.muted{color:#64748b}
</style>
</head>
<body>
<header><span class="title">ACCELERANDO</span><span class="tag">experiment · multi-run comparison</span></header>
<main>
  <div class="cards" id="cards"></div>
  <table><thead><tr><th>run</th><th>strategy</th><th>net pnl</th><th>return</th><th>trades</th><th>win</th><th>pf</th><th>sharpe</th><th>max dd</th></tr></thead><tbody id="runs"></tbody></table>
</main>
<script>
const fmt=(n,d=2)=>(n===null||n===undefined||!isFinite(n))?"-":Number(n).toLocaleString(undefined,{maximumFractionDigits:d,minimumFractionDigits:d});
const cls=n=>n>=0?"pos":"neg";
async function init(){
  const exp=await (await fetch("/api/experiment")).json();
  const runs=exp.runs||[];
  const best=[...runs].sort((a,b)=>b.metrics.net_pnl-a.metrics.net_pnl)[0];
  const totalTrades=runs.reduce((a,r)=>a+r.metrics.trades,0);
  document.getElementById("cards").innerHTML=[
    ["Runs",runs.length,""],["Best",best?best.label:"-",""],["Best PnL",best?"$"+fmt(best.metrics.net_pnl):"-",best?cls(best.metrics.net_pnl):""],["Total Trades",totalTrades,""]
  ].map(([k,v,c])=>`<div class="card"><div class="k">${k}</div><div class="v ${c}">${v}</div></div>`).join("");
  document.getElementById("runs").innerHTML=runs.sort((a,b)=>b.metrics.net_pnl-a.metrics.net_pnl).map(r=>{
    const m=r.metrics;
    return `<tr onclick="location.href='/run?id=${encodeURIComponent(r.id)}'"><td><b>${r.label}</b><div class="muted">${r.id}</div></td><td>${r.strategy}</td><td class="${cls(m.net_pnl)}">$${fmt(m.net_pnl)}</td><td class="${cls(m.return_pct)}">${fmt(m.return_pct)}%</td><td>${m.trades}</td><td>${fmt(m.win_rate*100,1)}%</td><td>${fmt(m.profit_factor)}</td><td class="${cls(m.sharpe)}">${fmt(m.sharpe)}</td><td class="neg">$${fmt(m.max_drawdown)}</td></tr>`;
  }).join("");
}
init();
</script>
</body>
</html>"#;

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

/// Start a multi-run experiment viewer backed by complete in-memory results.
pub fn serve_experiment(experiment: ExperimentResult, port: u16) -> std::io::Result<()> {
    let summaries: Vec<ExperimentRunSummary> =
        experiment.runs.iter().map(|run| run.summary.clone()).collect();
    serve_experiment_lazy(summaries, port, move |id| {
        experiment
            .runs
            .iter()
            .find(|run| run.summary.id == id)
            .map(|run| run.result.clone())
    })
}

/// Start a multi-run experiment viewer. The closure is called when a run chart is opened.
pub fn serve_experiment_lazy<F>(
    summaries: Vec<ExperimentRunSummary>,
    port: u16,
    load_result: F,
) -> std::io::Result<()>
where
    F: Fn(&str) -> Option<BacktestResult>,
{
    #[derive(Serialize)]
    struct SummaryPayload<'a> {
        runs: &'a [ExperimentRunSummary],
    }

    let summary_json =
        serde_json::to_string(&SummaryPayload { runs: &summaries }).expect("serialize summaries");
    let addr = format!("0.0.0.0:{port}");
    let server = Server::http(&addr)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("bind {addr}: {e}")))?;
    println!("Accelerando experiment viewer → http://localhost:{port}  (Ctrl+C to stop)");

    for request in server.incoming_requests() {
        let raw_url = request.url().to_string();
        let path = raw_url.split('?').next().unwrap_or("/");
        let response = match path {
            "/" | "/index.html" => html_response(EXPERIMENT_HTML),
            "/run" | "/run.html" => {
                let run_id = query_param(&raw_url, "id").unwrap_or_default();
                html_response(&studio_html_for_run(&run_id, &summaries))
            }
            "/api/experiment" => json_response(&summary_json),
            "/api/result" | "/result.json" => {
                let run_id = query_param(&raw_url, "id")
                    .or_else(|| summaries.first().map(|run| run.id.clone()))
                    .unwrap_or_default();
                match load_result(&run_id) {
                    Some(result) => json_response(
                        &serde_json::to_string(&result).expect("serialize selected result"),
                    ),
                    None => Response::from_string("run not found").with_status_code(404),
                }
            }
            "/api/progress" => json_response(r#"{"running":false,"has_result":true}"#),
            _ => Response::from_string("not found").with_status_code(404),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

fn studio_html_for_run(run_id: &str, runs: &[ExperimentRunSummary]) -> String {
    let escaped = json_string(run_id);
    let strategy = runs
        .iter()
        .find(|run| run.id == run_id)
        .map(|run| run.strategy.as_str())
        .unwrap_or("");
    let escaped_strategy = json_string(strategy);
    STUDIO_HTML.replace(
        "const price=$(\"price\"), pctx=price.getContext(\"2d\");",
        &format!(
            "const RUN_ID={escaped};\nconst RUN_STRATEGY={escaped_strategy};\nconst price=$(\"price\"), pctx=price.getContext(\"2d\");"
        ),
    )
    .replace("fetch(\"/api/result\")", "fetch(\"/api/result?id=\"+encodeURIComponent(RUN_ID))")
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(a * 16 + b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serialize json string")
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
