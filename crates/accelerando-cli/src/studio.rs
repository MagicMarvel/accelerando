//! Interactive backtest studio: a small HTTP server that runs backtests on demand from a config
//! posted by the page, reports live progress, serves the result, and persists runs to disk so they
//! can be reloaded later.
//!
//! The backtest executes on a worker thread while the request loop keeps answering `/api/progress`,
//! so the page can render a progress bar driven by bytes-read from the input feed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use accelerando_core::{
    run_backtest_progress, BrokerConfig, ParamSpec, ParamValue, Params, Pipeline, ProgressHandle,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tiny_http::{Header, Response, Server};

use crate::config::{params_from_table, BrokerCfg, RunConfig};

// ---------------------------------------------------------------------------------------------
// Config (JSON-friendly mirror of RunConfig; params are plain scalar maps the form round-trips).
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StudioStage {
    pub adapter: String,
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StudioConfig {
    #[serde(default = "yes")]
    pub keep_footprints: bool,
    pub data: StudioStage,
    pub aggregator: StudioStage,
    #[serde(default)]
    pub indicator: Vec<StudioStage>,
    pub strategy: StudioStage,
    #[serde(default)]
    pub broker: BrokerCfg,
}

fn yes() -> bool {
    true
}

impl StudioConfig {
    /// Seed a studio config from a `run.toml` (so the form opens pre-filled).
    pub fn from_run_config(rc: &RunConfig) -> StudioConfig {
        let stage = |adapter: &str, params: &toml::Table| StudioStage {
            adapter: adapter.to_string(),
            params: params_from_table(params).0,
        };
        StudioConfig {
            keep_footprints: true,
            data: stage(&rc.data.adapter, &rc.data.params),
            aggregator: stage(&rc.aggregator.adapter, &rc.aggregator.params),
            indicator: rc
                .indicator
                .iter()
                .map(|s| stage(&s.adapter, &s.params))
                .collect(),
            strategy: stage(&rc.strategy.adapter, &rc.strategy.params),
            broker: rc.broker.clone(),
        }
    }

    fn build_pipeline(&self, keep_footprints: bool) -> Result<Pipeline, String> {
        let source = accelerando_sources::build(&self.data.adapter, &Params(self.data.params.clone()))
            .ok_or_else(|| format!("unknown data source: {}", self.data.adapter))?;
        let aggregator =
            accelerando_aggregators::build(&self.aggregator.adapter, &Params(self.aggregator.params.clone()))
                .ok_or_else(|| format!("unknown aggregator: {}", self.aggregator.adapter))?;
        let mut indicators = Vec::new();
        for ind in &self.indicator {
            indicators.push(
                accelerando_indicators::build(&ind.adapter, &Params(ind.params.clone()))
                    .ok_or_else(|| format!("unknown indicator: {}", ind.adapter))?,
            );
        }
        let strategy =
            accelerando_strategy::build(&self.strategy.adapter, &Params(self.strategy.params.clone()))
                .ok_or_else(|| format!("unknown strategy: {}", self.strategy.adapter))?;
        Ok(Pipeline {
            source,
            aggregator,
            indicators,
            strategy,
            broker_cfg: BrokerConfig {
                commission_per_contract: self.broker.commission_per_contract,
                slippage_ticks: self.broker.slippage_ticks,
                starting_equity: self.broker.starting_equity,
            },
            keep_footprints,
        })
    }

    fn data_path(&self) -> Option<String> {
        self.data.params.get("path").and_then(|v| v.as_str()).map(str::to_string)
    }
}

/// A default seed config (first registered adapter of each kind, empty params). The form fills the
/// inputs from the schema defaults; the user supplies the data file path.
pub fn default_config() -> StudioConfig {
    let stage = |adapter: &str| StudioStage {
        adapter: adapter.to_string(),
        params: BTreeMap::new(),
    };
    StudioConfig {
        keep_footprints: true,
        data: stage(accelerando_sources::list().first().copied().unwrap_or("bookmap_csv")),
        aggregator: stage(accelerando_aggregators::list().first().copied().unwrap_or("time")),
        indicator: vec![stage(
            accelerando_indicators::list().first().copied().unwrap_or("whitesnake"),
        )],
        strategy: stage(
            accelerando_strategy::list().first().copied().unwrap_or("regime_follow"),
        ),
        broker: BrokerCfg::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// Server state.
// ---------------------------------------------------------------------------------------------

struct Studio {
    schema: String,
    config: StudioConfig,
    result_json: Option<String>,
    progress: ProgressHandle,
    running: bool,
    error: Option<String>,
    runs_dir: PathBuf,
}

/// Start the studio server (blocks). `seed` pre-fills the config form.
pub fn serve(seed: StudioConfig, runs_dir: PathBuf, port: u16) -> Result<(), String> {
    std::fs::create_dir_all(&runs_dir).ok();
    let state = Arc::new(Mutex::new(Studio {
        schema: schema_json(),
        config: seed,
        result_json: None,
        progress: ProgressHandle::new(),
        running: false,
        error: None,
        runs_dir,
    }));

    let addr = format!("0.0.0.0:{port}");
    let server = Server::http(&addr).map_err(|e| format!("bind {addr}: {e}"))?;
    println!("Accelerando studio → http://localhost:{port}  (Ctrl+C to stop)");

    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();
        let method = req.method().as_str().to_string();

        // Read the request body up front (only POSTs carry one).
        let body = if method == "POST" {
            let mut b = String::new();
            let _ = std::io::Read::read_to_string(req.as_reader(), &mut b);
            b
        } else {
            String::new()
        };

        let response = match (method.as_str(), path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => html(accelerando_web::studio_html()),
            ("GET", "/api/schema") => json_str(&state.lock().unwrap().schema),
            ("GET", "/api/config") => {
                let s = state.lock().unwrap();
                json_str(&serde_json::to_string(&s.config).unwrap())
            }
            ("GET", "/api/progress") => progress_response(&state),
            ("GET", "/api/result") => {
                let s = state.lock().unwrap();
                match &s.result_json {
                    Some(r) => json_str(r),
                    None => Response::from_string("no result".to_string()).with_status_code(404),
                }
            }
            ("GET", "/api/runs") => json_str(&list_runs(&state.lock().unwrap().runs_dir)),
            ("POST", "/api/backtest") => start_backtest(&state, &body),
            ("POST", "/api/save") => save_run(&state, &body),
            ("GET", "/api/load") => load_run(&state, query_param(&url, "name")),
            _ => Response::from_string("not found".to_string()).with_status_code(404),
        };
        let _ = req.respond(response);
    }
    Ok(())
}

fn start_backtest(state: &Arc<Mutex<Studio>>, body: &str) -> Resp {
    let cfg: StudioConfig = match serde_json::from_str(body) {
        Ok(c) => c,
        Err(e) => return json_str(&json!({"ok": false, "error": format!("bad config: {e}")}).to_string()),
    };

    // Validate the input feed exists before spawning, since the engine would otherwise panic.
    if let Some(p) = cfg.data_path() {
        if !Path::new(&p).is_file() {
            return json_str(&json!({"ok": false, "error": format!("data file not found: {p}")}).to_string());
        }
    } else {
        return json_str(&json!({"ok": false, "error": "data source has no `path` parameter"}).to_string());
    }

    {
        let mut s = state.lock().unwrap();
        if s.running {
            return json_str(&json!({"ok": false, "error": "a backtest is already running"}).to_string());
        }
        let progress = ProgressHandle::new();
        if let Some(p) = cfg.data_path() {
            if let Ok(md) = std::fs::metadata(&p) {
                progress.set_total_bytes(md.len());
            }
        }
        s.progress = progress;
        s.running = true;
        s.error = None;
        s.config = cfg.clone();
    }

    let st = state.clone();
    let progress = state.lock().unwrap().progress.clone();
    std::thread::spawn(move || {
        let outcome = cfg
            .build_pipeline(cfg.keep_footprints)
            .map(|pl| run_backtest_progress(pl, Some(progress.clone())));
        let mut s = st.lock().unwrap();
        s.running = false;
        match outcome {
            Ok(result) => match serde_json::to_string(&result) {
                Ok(j) => s.result_json = Some(j),
                Err(e) => s.error = Some(format!("serialize result: {e}")),
            },
            Err(e) => s.error = Some(e),
        }
        progress.finish();
    });

    json_str(&json!({"ok": true}).to_string())
}

fn progress_response(state: &Arc<Mutex<Studio>>) -> Resp {
    let s = state.lock().unwrap();
    let snap = s.progress.snapshot();
    let pct = snap.fraction();
    json_str(
        &json!({
            "running": s.running,
            "done": snap.done,
            "bytes": snap.bytes,
            "total": snap.total_bytes,
            "footprints": snap.footprints,
            "pct": pct,
            "error": s.error,
            "has_result": s.result_json.is_some(),
        })
        .to_string(),
    )
}

fn save_run(state: &Arc<Mutex<Studio>>, body: &str) -> Resp {
    let name = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_default();
    let name = sanitize(&name);
    if name.is_empty() {
        return json_str(&json!({"ok": false, "error": "empty name"}).to_string());
    }
    let s = state.lock().unwrap();
    let Some(result) = &s.result_json else {
        return json_str(&json!({"ok": false, "error": "no result to save"}).to_string());
    };
    let dir = s.runs_dir.join(&name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return json_str(&json!({"ok": false, "error": format!("{e}")}).to_string());
    }
    let cfg_json = serde_json::to_string_pretty(&s.config).unwrap_or_default();
    let _ = std::fs::write(dir.join("config.json"), cfg_json);
    if let Err(e) = std::fs::write(dir.join("result.json"), result) {
        return json_str(&json!({"ok": false, "error": format!("{e}")}).to_string());
    }
    json_str(&json!({"ok": true, "name": name}).to_string())
}

fn load_run(state: &Arc<Mutex<Studio>>, name: Option<String>) -> Resp {
    let Some(name) = name.map(|n| sanitize(&n)).filter(|n| !n.is_empty()) else {
        return json_str(&json!({"ok": false, "error": "missing name"}).to_string());
    };
    let mut s = state.lock().unwrap();
    let dir = s.runs_dir.join(&name);
    let result = match std::fs::read_to_string(dir.join("result.json")) {
        Ok(r) => r,
        Err(e) => return json_str(&json!({"ok": false, "error": format!("{e}")}).to_string()),
    };
    if let Ok(cfg_text) = std::fs::read_to_string(dir.join("config.json")) {
        if let Ok(cfg) = serde_json::from_str::<StudioConfig>(&cfg_text) {
            s.config = cfg;
        }
    }
    s.result_json = Some(result);
    s.error = None;
    json_str(&json!({"ok": true, "name": name}).to_string())
}

fn list_runs(runs_dir: &Path) -> String {
    let mut runs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(runs_dir) {
        for e in entries.flatten() {
            let dir = e.path();
            let result = dir.join("result.json");
            if !result.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            // Pull a tiny summary from the saved metrics without loading footprints.
            let (net_pnl, trades, sharpe) = std::fs::read_to_string(&result)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("metrics").cloned())
                .map(|m| {
                    (
                        m.get("net_pnl").and_then(|x| x.as_f64()).unwrap_or(0.0),
                        m.get("trades").and_then(|x| x.as_u64()).unwrap_or(0),
                        m.get("sharpe").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    )
                })
                .unwrap_or((0.0, 0, 0.0));
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            runs.push(json!({
                "name": name, "net_pnl": net_pnl, "trades": trades,
                "sharpe": sharpe, "modified": modified,
            }));
        }
    }
    runs.sort_by(|a, b| {
        b["modified"].as_u64().cmp(&a["modified"].as_u64())
    });
    serde_json::to_string(&runs).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------------------------
// Schema (adapters + their parameter specs) for the auto-generated config form.
// ---------------------------------------------------------------------------------------------

fn schema_json() -> String {
    fn specs(names: &[&str], get: impl Fn(&str) -> Option<ParamSpec>) -> Vec<serde_json::Value> {
        names
            .iter()
            .map(|n| json!({ "name": n, "params": get(n) }))
            .collect()
    }
    json!({
        "sources": specs(accelerando_sources::list(), accelerando_sources::spec),
        "aggregators": specs(accelerando_aggregators::list(), accelerando_aggregators::spec),
        "indicators": specs(accelerando_indicators::list(), accelerando_indicators::spec),
        "strategies": specs(accelerando_strategy::list(), accelerando_strategy::spec),
    })
    .to_string()
}

// ---------------------------------------------------------------------------------------------
// HTTP helpers.
// ---------------------------------------------------------------------------------------------

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn ctype(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).unwrap()
}

fn html(body: &str) -> Resp {
    let mut r = Response::from_string(body.to_string());
    r.add_header(ctype("text/html; charset=utf-8"));
    r
}

fn json_str(body: &str) -> Resp {
    let mut r = Response::from_string(body.to_string());
    r.add_header(ctype("application/json; charset=utf-8"));
    r
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split('?').nth(1)?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().map(|v| urldecode(v));
        }
    }
    None
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
        .collect::<String>()
        .trim()
        .to_string()
}
