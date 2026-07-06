//! Self-contained web UI helpers for embedding a read-only backtest result viewer.
//!
//! No node, no build step: pass a [`BacktestResult`] to [`serve`] and open the printed localhost URL.

mod replay;

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use accelerando_core::{result::{ExperimentResult, ExperimentRunSummary}, BacktestResult};
use serde::Serialize;
use serde_json::Value;
use tiny_http::{Header, Method, Request, Response, Server};

pub use replay::ReplayManager;

const STUDIO_HTML: &str = include_str!("studio.html");
pub(crate) const EXPERIMENT_HTML: &str = include_str!("experiment.html");

/// The embedded result/studio page HTML.
///
/// Applications can reuse this if they want to provide their own JSON API around the page.
pub fn studio_html() -> &'static str {
    STUDIO_HTML
}

/// Generic chart-annotation settings for the embedded studio.
///
/// The web crate only draws and persists labeled price/time boxes. Host applications decide what
/// labels mean and how to consume the JSONL afterwards.
#[derive(Clone, Debug, Serialize)]
pub struct AnnotationConfig {
    pub enabled: bool,
    pub labels: Vec<String>,
    pub save_path: String,
}

impl AnnotationConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            labels: Vec::new(),
            save_path: String::new(),
        }
    }

    pub fn new(
        labels: impl IntoIterator<Item = impl Into<String>>,
        save_path: impl Into<String>,
    ) -> Self {
        Self {
            enabled: true,
            labels: labels.into_iter().map(Into::into).collect(),
            save_path: save_path.into(),
        }
    }
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
    serve_experiment_lazy_heatmap(summaries, port, load_result, |_| None)
}

/// Like [`serve_experiment_lazy`], but also routes `GET /api/heatmap?<query>` to `heatmap`, which
/// receives the raw query string and returns a ready-to-send JSON body (or `None` for 404). This
/// lets the host app stream a windowed, zoomable order-book heatmap without the web crate needing to
/// know its data model.
pub fn serve_experiment_lazy_heatmap<F, H>(
    summaries: Vec<ExperimentRunSummary>,
    port: u16,
    load_result: F,
    heatmap: H,
) -> std::io::Result<()>
where
    F: Fn(&str) -> Option<BacktestResult>,
    H: Fn(&str) -> Option<String>,
{
    serve_experiment_lazy_heatmap_with_annotations(
        summaries,
        port,
        load_result,
        heatmap,
        AnnotationConfig::disabled(),
    )
}

/// Like [`serve_experiment_lazy_heatmap`], with optional generic chart annotations enabled.
pub fn serve_experiment_lazy_heatmap_with_annotations<F, H>(
    summaries: Vec<ExperimentRunSummary>,
    port: u16,
    load_result: F,
    heatmap: H,
    annotation_config: AnnotationConfig,
) -> std::io::Result<()>
where
    F: Fn(&str) -> Option<BacktestResult>,
    H: Fn(&str) -> Option<String>,
{
    serve_experiment_dashboard(summaries, port, load_result, heatmap, annotation_config, None)
}

/// Like [`serve_experiment_lazy_heatmap_with_annotations`], additionally serving an interactive
/// manual bar-replay session at `/replay`: paper-trade market/limit/breakout orders bar by bar,
/// with each session persisted by [`ReplayManager`].
pub fn serve_experiment_lazy_heatmap_with_replay<F, H>(
    summaries: Vec<ExperimentRunSummary>,
    port: u16,
    load_result: F,
    heatmap: H,
    annotation_config: AnnotationConfig,
    replay: ReplayManager,
) -> std::io::Result<()>
where
    F: Fn(&str) -> Option<BacktestResult>,
    H: Fn(&str) -> Option<String>,
{
    serve_experiment_dashboard(
        summaries,
        port,
        load_result,
        heatmap,
        annotation_config,
        Some(replay),
    )
}

fn serve_experiment_dashboard<F, H>(
    summaries: Vec<ExperimentRunSummary>,
    port: u16,
    load_result: F,
    heatmap: H,
    annotation_config: AnnotationConfig,
    replay: Option<ReplayManager>,
) -> std::io::Result<()>
where
    F: Fn(&str) -> Option<BacktestResult>,
    H: Fn(&str) -> Option<String>,
{
    #[derive(Serialize)]
    struct SummaryPayload<'a> {
        runs: &'a [ExperimentRunSummary],
    }

    let summary_json =
        serde_json::to_string(&SummaryPayload { runs: &summaries }).expect("serialize summaries");
    let annotation_json =
        serde_json::to_string(&annotation_config).expect("serialize annotation config");
    let index_html = if replay.is_some() {
        replay::experiment_html_with_replay_button()
    } else {
        EXPERIMENT_HTML.to_string()
    };
    let addr = format!("0.0.0.0:{port}");
    let server = Server::http(&addr)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("bind {addr}: {e}")))?;
    println!("Accelerando experiment viewer → http://localhost:{port}  (Ctrl+C to stop)");

    for mut request in server.incoming_requests() {
        let raw_url = request.url().to_string();
        let path = raw_url.split('?').next().unwrap_or("/");
        let method = request.method().clone();
        let response = match (method, path) {
            (Method::Get, "/" | "/index.html") => html_response(&index_html),
            (Method::Get, "/run" | "/run.html") => {
                let run_id = query_param(&raw_url, "id").unwrap_or_default();
                html_response(&studio_html_for_run(
                    &run_id,
                    &summaries,
                    &annotation_json,
                ))
            }
            (Method::Get, "/replay" | "/replay.html") => match &replay {
                Some(_) => {
                    let run_id = query_param(&raw_url, "id").unwrap_or_default();
                    html_response(&replay::replay_html(&run_id, &annotation_json))
                }
                None => Response::from_string("replay disabled").with_status_code(404),
            },
            (Method::Get, "/api/experiment") => json_response(&summary_json),
            (Method::Get, "/api/result" | "/result.json") => {
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
            (Method::Get, "/api/heatmap") => {
                let query = raw_url.split_once('?').map(|(_, q)| q).unwrap_or("");
                let id = query_param_from_query(query, "id").unwrap_or_default();
                let replay_blocked = id.starts_with("replay_")
                    && !replay
                        .as_ref()
                        .is_some_and(|r| r.validate_heatmap_query(&id, query));
                if replay_blocked {
                    Response::from_string("no replay heatmap beyond current bar")
                        .with_status_code(404)
                } else {
                    match heatmap(query) {
                        Some(body) => json_response(&body),
                        None => Response::from_string("no heatmap").with_status_code(404),
                    }
                }
            }
            (Method::Post, "/api/replay/new") => match &replay {
                Some(replay) => match replay.create_session() {
                    Ok(state) => json_response(
                        &serde_json::json!({ "ok": true, "id": state.id, "url": format!("/replay?id={}", state.id), "replay": state })
                            .to_string(),
                    ),
                    Err(e) => json_error(400, &e),
                },
                None => json_error(404, "replay disabled"),
            },
            (Method::Get, "/api/replay/state") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay.state_value(&id) {
                        Ok(value) => json_response(&value.to_string()),
                        Err(e) => json_error(404, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/order") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay::read_json(&mut request)
                        .and_then(|body| replay.place_order(&id, body))
                    {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/step") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    let body = replay::read_json(&mut request)
                        .unwrap_or(replay::StepRequest { count: Some(1) });
                    match replay.advance(&id, body.count.unwrap_or(1)) {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/update") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay::read_json(&mut request)
                        .and_then(|body| replay.update_active_order(&id, body))
                    {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/cancel") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay.cancel_pending(&id) {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/flatten") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay.flatten(&id) {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Get, "/api/annotations") | (Method::Post, "/api/annotations") => {
                let run_id = query_param(&raw_url, "id").unwrap_or_default();
                handle_annotations(
                    &annotation_config,
                    &run_id,
                    request.method().clone(),
                    &mut request,
                )
            }
            (Method::Get, "/api/progress") => json_response(r#"{"running":false,"has_result":true}"#),
            _ => Response::from_string("not found").with_status_code(404),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

fn studio_html_for_run(
    run_id: &str,
    runs: &[ExperimentRunSummary],
    annotation_json: &str,
) -> String {
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
            "globalThis.RUN_ID={escaped};\nglobalThis.RUN_STRATEGY={escaped_strategy};\nglobalThis.RUN_SUMMARY={summary_json};\nglobalThis.ANNOTATION_CONFIG={annotation_json};\nconst price=$(\"price\"), pctx=price.getContext(\"2d\");"
        ),
    )
    .replace(
        "fetch(\"/api/result\")",
        "fetch(\"/api/result?id=\"+encodeURIComponent(globalThis.RUN_ID||\"\"))",
    )
}

fn handle_annotations(
    cfg: &AnnotationConfig,
    run_id: &str,
    method: Method,
    request: &mut Request,
) -> Response<Cursor<Vec<u8>>> {
    if !cfg.enabled {
        return Response::from_string("annotations disabled").with_status_code(404);
    }
    match method {
        Method::Get => match load_annotations(cfg, run_id) {
            Ok(annotations) => {
                json_response(&serde_json::json!({ "annotations": annotations }).to_string())
            }
            Err(e) => json_error(500, &format!("load annotations: {e}")),
        },
        Method::Post => match append_annotation(cfg, run_id, request) {
            Ok(annotation) => json_response(
                &serde_json::json!({ "ok": true, "annotation": annotation }).to_string(),
            ),
            Err(e) => json_error(400, &e),
        },
        _ => Response::from_string("method not allowed").with_status_code(405),
    }
}

fn load_annotations(cfg: &AnnotationConfig, run_id: &str) -> std::io::Result<Vec<Value>> {
    let path = Path::new(&cfg.save_path);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let mut annotations = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let matches_run = run_id.is_empty()
            || value
                .get("run")
                .and_then(Value::as_str)
                .is_some_and(|run| run == run_id);
        if matches_run {
            annotations.push(value);
        }
    }
    Ok(annotations)
}

fn append_annotation(
    cfg: &AnnotationConfig,
    run_id: &str,
    request: &mut Request,
) -> Result<Value, String> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("read annotation body: {e}"))?;
    let mut annotation: Value =
        serde_json::from_str(&body).map_err(|e| format!("parse annotation json: {e}"))?;
    let obj = annotation
        .as_object_mut()
        .ok_or_else(|| "annotation must be a JSON object".to_string())?;
    obj.insert("run".to_string(), Value::String(run_id.to_string()));
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system time: {e}"))?
        .as_millis()
        .min(u64::MAX as u128) as u64;
    obj.insert(
        "created_at_ms".to_string(),
        Value::Number(created_at_ms.into()),
    );

    let path = Path::new(&cfg.save_path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("create annotation dir: {e}"))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open annotation file: {e}"))?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&annotation).map_err(|e| format!("serialize annotation: {e}"))?
    )
    .map_err(|e| format!("write annotation: {e}"))?;
    Ok(annotation)
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query_param_from_query(query, key)
}

pub(crate) fn query_param_from_query(query: &str, key: &str) -> Option<String> {
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

pub(crate) fn json_string(value: &str) -> String {
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

fn json_error(status: u16, message: &str) -> Response<Cursor<Vec<u8>>> {
    let mut r = json_response(&serde_json::json!({ "ok": false, "error": message }).to_string());
    r = r.with_status_code(status);
    r
}

fn content_type(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).unwrap()
}
