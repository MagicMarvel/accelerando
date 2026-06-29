//! Self-contained web UI helpers for embedding a read-only backtest result viewer.
//!
//! No node, no build step: pass a [`BacktestResult`] to [`serve`] and open the printed localhost URL.

use std::io::Cursor;

use accelerando_core::{result::{ExperimentResult, ExperimentRunSummary}, BacktestResult};
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
.title{font-size:16px;font-weight:800;letter-spacing:.3px}.brand-home{color:#fff;text-decoration:none;flex:0 0 auto}.brand-home:hover{text-decoration:underline}.tag{font-size:12px;color:#94a3b8}
main{padding:14px}
.cards{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:12px}
.card{min-width:120px;padding:8px 10px;border:1px solid #e5e7eb;border-radius:8px;background:#fff}
.k{font-size:10px;color:#6b7280;text-transform:uppercase;letter-spacing:.4px}.v{font-size:18px;font-weight:800;margin-top:2px}
.runs{display:flex;flex-direction:column;gap:10px}
.run-card{border:1px solid #e5e7eb;border-radius:8px;background:#fff;overflow:hidden}
.run-top{display:grid;grid-template-columns:minmax(180px,1.2fr) minmax(120px,.8fr) repeat(7,minmax(78px,auto)) auto;gap:8px;align-items:center;padding:10px 12px;border-bottom:1px solid #eef2f7}
.run-name{font-size:14px;font-weight:800}.muted{color:#64748b}.strategy{font-size:13px;color:#334155}
.metric{text-align:right}.metric .k{font-size:9px}.metric .v{font-size:13px}
.open{justify-self:end;text-decoration:none;border:1px solid #c5ccd6;border-radius:8px;padding:6px 10px;color:#111827;background:#fff;font-size:12px;font-weight:700}
.open:hover{background:#eff6ff;border-color:#bfdbfe}
.params{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:8px;padding:10px 12px;background:#fbfcfe}
.param-group{border:1px solid #eef2f7;border-radius:8px;background:#fff;padding:8px}
.param-title{font-size:10px;color:#475569;text-transform:uppercase;letter-spacing:.4px;font-weight:800;margin-bottom:6px}
.param-list{display:flex;flex-wrap:wrap;gap:6px}
.param{display:inline-flex;gap:4px;align-items:baseline;border:1px solid #e5e7eb;border-radius:6px;padding:3px 6px;background:#f8fafc;font-size:12px;max-width:100%}
.param b{color:#475569;font-weight:600;overflow:hidden;text-overflow:ellipsis}.param span{font-family:Consolas,"SFMono-Regular",monospace;color:#111827;overflow:hidden;text-overflow:ellipsis}
.pos{color:#2563eb}.neg{color:#d97706}
@media(max-width:1100px){.run-top{grid-template-columns:minmax(180px,1fr) repeat(2,minmax(88px,auto));}.strategy{grid-column:1/-1}.open{grid-column:1/-1;justify-self:start}.metric{text-align:left}}
</style>
</head>
<body>
<header><a class="title brand-home" href="/" title="Back to home">ACCELERANDO</a><span class="tag">experiment · multi-run comparison</span></header>
<main>
  <div class="cards" id="cards"></div>
  <div class="runs" id="runs"></div>
</main>
<script>
const fmt=(n,d=2)=>(n===null||n===undefined||!isFinite(n))?"-":Number(n).toLocaleString(undefined,{maximumFractionDigits:d,minimumFractionDigits:d});
const cls=n=>n>=0?"pos":"neg";
const html=s=>String(s??"").replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#39;"}[c]));
function fmtParam(v){
  if(v===null||v===undefined)return "-";
  if(typeof v==="number")return Number.isInteger(v)?String(v):fmt(v,4);
  if(typeof v==="boolean")return v?"true":"false";
  return html(v);
}
function flattenParams(value,prefix="",out=[]){
  if(value===null||value===undefined)return out;
  if(Array.isArray(value)){
    value.forEach((item,i)=>{
      flattenParams(item,prefix||String(i+1),out);
    });
    return out;
  }
  if(typeof value==="object"){
    if(value.adapter&&value.params&&typeof value.params==="object"){
      const base=prefix?`${prefix}.${value.adapter}`:value.adapter;
      flattenParams(value.params,base,out);
      return out;
    }
    for(const [k,v] of Object.entries(value)){
      if(k==="adapter")continue;
      flattenParams(v,prefix?`${prefix}.${k}`:k,out);
    }
    return out;
  }
  out.push({key:prefix,value});
  return out;
}
function groupParams(items){
  const groups=[];
  for(const item of items){
    const parts=item.key.split(".");
    const group=parts.slice(0,-1).join(".")||"params";
    const name=parts[parts.length-1]||item.key;
    let g=groups.find(x=>x.name===group);
    if(!g){ g={name:group,items:[]}; groups.push(g); }
    g.items.push({name,value:item.value});
  }
  return groups;
}
function metric(k,v,c=""){
  return `<div class="metric"><div class="k">${k}</div><div class="v ${c}">${v}</div></div>`;
}
function renderParams(r){
  return groupParams(r._flatParams).map(g=>`<div class="param-group"><div class="param-title">${html(g.name)}</div><div class="param-list">${g.items.map(p=>`<span class="param" title="${html(p.name)} = ${html(fmtParam(p.value))}"><b>${html(p.name)}</b><span>${fmtParam(p.value)}</span></span>`).join("")}</div></div>`).join("");
}
async function init(){
  const exp=await (await fetch("/api/experiment")).json();
  const runs=exp.runs||[];
  for(const r of runs) r._flatParams=flattenParams(r.params||{});
  const best=[...runs].sort((a,b)=>b.metrics.net_pnl-a.metrics.net_pnl)[0];
  const totalTrades=runs.reduce((a,r)=>a+r.metrics.trades,0);
  document.getElementById("cards").innerHTML=[
    ["Runs",runs.length,""],["Best",best?best.label:"-",""],["Best PnL",best?"$"+fmt(best.metrics.net_pnl):"-",best?cls(best.metrics.net_pnl):""],["Total Trades",totalTrades,""]
  ].map(([k,v,c])=>`<div class="card"><div class="k">${k}</div><div class="v ${c}">${v}</div></div>`).join("");
  document.getElementById("runs").innerHTML=runs.sort((a,b)=>b.metrics.net_pnl-a.metrics.net_pnl).map(r=>{
    const m=r.metrics;
    return `<section class="run-card">
      <div class="run-top">
        <div><div class="run-name">${html(r.label)}</div><div class="muted">${html(r.id)}</div></div>
        <div class="strategy">${html(r.strategy)}</div>
        ${metric("net pnl","$"+fmt(m.net_pnl),cls(m.net_pnl))}
        ${metric("return",fmt(m.return_pct)+"%",cls(m.return_pct))}
        ${metric("trades",m.trades)}
        ${metric("win",fmt(m.win_rate*100,1)+"%")}
        ${metric("pf",fmt(m.profit_factor))}
        ${metric("sharpe",fmt(m.sharpe),cls(m.sharpe))}
        ${metric("max dd","$"+fmt(m.max_drawdown),"neg")}
        <a class="open" href="/run?id=${encodeURIComponent(r.id)}">Open chart</a>
      </div>
      <div class="params">${renderParams(r)}</div>
    </section>`;
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
    let summary = runs.iter().find(|run| run.id == run_id);
    let strategy = summary.map(|run| run.strategy.as_str()).unwrap_or("");
    let escaped_strategy = json_string(strategy);
    let summary_json = summary
        .map(|run| serde_json::to_string(run).expect("serialize run summary"))
        .unwrap_or_else(|| "null".to_string());
    STUDIO_HTML.replace(
        "const price=$(\"price\"), pctx=price.getContext(\"2d\");",
        &format!(
            "const RUN_ID={escaped};\nconst RUN_STRATEGY={escaped_strategy};\nconst RUN_SUMMARY={summary_json};\nconst price=$(\"price\"), pctx=price.getContext(\"2d\");"
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
