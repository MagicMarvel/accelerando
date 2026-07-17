//! Self-contained web UI helpers for embedding a read-only backtest result viewer.
//!
//! No node, no build step: pass a [`BacktestResult`] to [`serve`] and open the printed localhost URL.

mod replay;

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use accelerando_core::{
    result::{EquityPoint, ExperimentResult, ExperimentRunSummary, LiquidityHeatmap, Series, Trade},
    BacktestResult, Footprint, Level, Metrics, VpLevel,
};
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

/// The browser workbench, configured through one builder instead of a ladder of `serve_*`
/// variants: start from [`Studio::new`] (or [`Studio::experiment`] for an in-memory
/// [`ExperimentResult`]), chain whatever capabilities the host app has, then call
/// [`Studio::serve`].
///
/// ```no_run
/// # let summaries = Vec::new();
/// # let annotations = accelerando_web::AnnotationConfig::disabled();
/// accelerando_web::Studio::new()
///     .runs(summaries, |_id| None)
///     .heatmap(|_query| None)
///     .annotations(annotations)
///     .serve(8888)
///     .unwrap();
/// ```
pub struct Studio {
    summaries: Vec<ExperimentRunSummary>,
    load_result: Box<dyn Fn(&str) -> Option<BacktestResult>>,
    load_ladders: Box<dyn Fn(&str, usize, usize) -> Option<Vec<Vec<Level>>>>,
    load_volume_profile: Box<dyn Fn(&str, usize, usize) -> Option<Vec<VpLevel>>>,
    heatmap: Box<dyn Fn(&str) -> Option<String>>,
    annotations: AnnotationConfig,
    replay: Option<ReplayManager>,
}

impl Default for Studio {
    fn default() -> Self {
        Self::new()
    }
}

impl Studio {
    /// An empty workbench: no runs, no heatmap, no annotations, no replay.
    pub fn new() -> Self {
        Self {
            summaries: Vec::new(),
            load_result: Box::new(|_| None),
            load_ladders: Box::new(|_, _, _| None),
            load_volume_profile: Box::new(|_, _, _| None),
            heatmap: Box::new(|_| None),
            annotations: AnnotationConfig::disabled(),
            replay: None,
        }
    }

    /// Convenience for a complete in-memory experiment (results served eagerly).
    pub fn experiment(experiment: ExperimentResult) -> Self {
        let summaries: Vec<ExperimentRunSummary> =
            experiment.runs.iter().map(|run| run.summary.clone()).collect();
        Self::new().runs(summaries, move |id| {
            experiment
                .runs
                .iter()
                .find(|run| run.summary.id == id)
                .map(|run| run.result.clone())
        })
    }

    /// Run summaries for the index page plus a loader called when a run chart is opened.
    pub fn runs(
        mut self,
        summaries: Vec<ExperimentRunSummary>,
        load_result: impl Fn(&str) -> Option<BacktestResult> + 'static,
    ) -> Self {
        self.summaries = summaries;
        self.load_result = Box::new(load_result);
        self
    }

    /// Supply full footprint ladders for an exclusive bar range `[from, to)`. The chart calls
    /// this only for a small, footprint-readable visible window, allowing the initial result JSON
    /// to keep its ladders compacted.
    pub fn footprint_ladders(
        mut self,
        load_ladders: impl Fn(&str, usize, usize) -> Option<Vec<Vec<Level>>> + 'static,
    ) -> Self {
        self.load_ladders = Box::new(load_ladders);
        self
    }

    /// Supply an already-aggregated fixed-range volume profile for the inclusive bar range
    /// `[from, to]`. Aggregating server-side avoids transferring every ladder in a large selection.
    pub fn volume_profiles(
        mut self,
        load_volume_profile: impl Fn(&str, usize, usize) -> Option<Vec<VpLevel>> + 'static,
    ) -> Self {
        self.load_volume_profile = Box::new(load_volume_profile);
        self
    }

    /// Route `GET /api/heatmap?<query>` to the host: it receives the raw query string and
    /// returns a ready-to-send JSON body (or `None` for 404), so the web crate never needs
    /// to know the order-book data model.
    pub fn heatmap(mut self, heatmap: impl Fn(&str) -> Option<String> + 'static) -> Self {
        self.heatmap = Box::new(heatmap);
        self
    }

    /// Enable generic chart annotations, persisted to the configured JSONL file.
    pub fn annotations(mut self, config: AnnotationConfig) -> Self {
        self.annotations = config;
        self
    }

    /// Enable interactive manual bar replay at `/replay`, with sessions persisted by the
    /// [`ReplayManager`].
    pub fn replay(mut self, replay: ReplayManager) -> Self {
        self.replay = Some(replay);
        self
    }

    /// Bind the port and serve until the process is killed.
    pub fn serve(self, port: u16) -> std::io::Result<()> {
        serve_studio(self, port)
    }
}

fn serve_studio(studio: Studio, port: u16) -> std::io::Result<()> {
    #[derive(Serialize)]
    struct SummaryPayload<'a> {
        runs: &'a [ExperimentRunSummary],
    }
    let Studio {
        summaries,
        load_result,
        load_ladders,
        load_volume_profile,
        heatmap,
        annotations: annotation_config,
        replay,
    } = studio;

    let summary_json =
        serde_json::to_string(&SummaryPayload { runs: &summaries }).expect("serialize summaries");
    let annotation_json =
        serde_json::to_string(&annotation_config).expect("serialize annotation config");
    // The index page probes /api/replay/sessions itself and hides the manual-replay tab
    // when replay is disabled, so one static page serves both configurations.
    let index_html = EXPERIMENT_HTML.to_string();
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
                    .filter(|id| !id.is_empty())
                    .or_else(|| summaries.first().map(|run| run.id.clone()))
                    .unwrap_or_default();
                match load_result(&run_id) {
                    // `from` present → windowed page of footprints/equity (large results
                    // exceed the browser's ~512MB JSON string cap when sent whole).
                    // The first page (from == 0) additionally carries `meta` with every
                    // non-paged field. `from` absent keeps the legacy full payload.
                    // Pages serialize borrowed slices straight to a string — no
                    // serde_json::Value intermediate, which doubles per-page latency.
                    Some(result) => match query_usize(&raw_url, "from") {
                        Some(from) => {
                            let count =
                                query_usize(&raw_url, "count").unwrap_or(60_000).clamp(1, 200_000);
                            let fp_from = from.min(result.footprints.len());
                            let fp_to = fp_from.saturating_add(count).min(result.footprints.len());
                            let eq_from = query_usize(&raw_url, "eq_from")
                                .unwrap_or(0)
                                .min(result.equity.len());
                            let eq_to = eq_from.saturating_add(count).min(result.equity.len());
                            let page = ResultPage {
                                total_fps: result.footprints.len(),
                                total_eq: result.equity.len(),
                                footprints_from: fp_from,
                                footprints: &result.footprints[fp_from..fp_to],
                                equity_from: eq_from,
                                equity: &result.equity[eq_from..eq_to],
                                meta: (fp_from == 0).then(|| ResultMeta {
                                    metrics: &result.metrics,
                                    trades: &result.trades,
                                    series: &result.series,
                                    liquidity_heatmap: &result.liquidity_heatmap,
                                    tick_size: result.tick_size,
                                    multiplier: result.multiplier,
                                }),
                            };
                            json_response(
                                &serde_json::to_string(&page).expect("serialize result page"),
                            )
                        }
                        None => json_response(
                            &serde_json::to_string(&result).expect("serialize selected result"),
                        ),
                    },
                    None => Response::from_string("run not found").with_status_code(404),
                }
            }
            (Method::Get, "/api/footprint-ladders") => {
                const MAX_LADDER_WINDOW: usize = 2_000;
                let run_id = query_param(&raw_url, "id").unwrap_or_default();
                let from = query_usize(&raw_url, "from");
                let to = query_usize(&raw_url, "to");
                match (from, to) {
                    (Some(from), Some(to)) if to > from && to - from <= MAX_LADDER_WINDOW => {
                        match load_ladders(&run_id, from, to) {
                            Some(ladders) => json_response(
                                &serde_json::json!({ "from": from, "ladders": ladders })
                                    .to_string(),
                            ),
                            None => Response::from_string("footprint ladders unavailable")
                                .with_status_code(404),
                        }
                    }
                    _ => json_error(
                        400,
                        "from/to must define a non-empty range of at most 2000 bars",
                    ),
                }
            }
            (Method::Get, "/api/volume-profile") => {
                let run_id = query_param(&raw_url, "id").unwrap_or_default();
                let from = query_usize(&raw_url, "from");
                let to = query_usize(&raw_url, "to");
                match (from, to) {
                    (Some(from), Some(to)) if to >= from => {
                        match load_volume_profile(&run_id, from, to) {
                            Some(levels) => json_response(
                                &serde_json::json!({
                                    "from": from,
                                    "to": to,
                                    "levels": levels
                                })
                                .to_string(),
                            ),
                            None => Response::from_string("volume profile unavailable")
                                .with_status_code(404),
                        }
                    }
                    _ => json_error(400, "from/to must define a valid inclusive range"),
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
                Some(replay) => {
                    let body = replay::read_json(&mut request).unwrap_or_default();
                    match replay.create_session(body) {
                        Ok(state) => json_response(
                            &serde_json::json!({ "ok": true, "id": state.id, "url": format!("/replay?id={}", state.id), "replay": state })
                                .to_string(),
                        ),
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Get, "/api/replay/sessions") => match &replay {
                Some(replay) => json_response(
                    &serde_json::json!({ "sessions": replay.list_sessions() }).to_string(),
                ),
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/delete") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay.delete_session(&id) {
                        Ok(()) => json_response(&serde_json::json!({ "ok": true }).to_string()),
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Get, "/api/replay/state") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    let since_fp = query_param(&raw_url, "since_fp")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let since_eq = query_param(&raw_url, "since_eq")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    match replay.state_value(&id, since_fp, since_eq) {
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
                    let body = replay::read_json(&mut request).unwrap_or(replay::StepRequest {
                        count: Some(1),
                        to_ts_ms: None,
                        stop_on_event: None,
                    });
                    let stop_on_event = body.stop_on_event.unwrap_or(true);
                    let stepped = match body.to_ts_ms {
                        Some(ts_ms) => {
                            replay.advance_to_ts(&id, ts_ms.saturating_mul(1_000_000), stop_on_event)
                        }
                        None => replay.advance(&id, body.count.unwrap_or(1), stop_on_event),
                    };
                    match stepped {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/back") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    let body: replay::BackRequest = replay::read_json(&mut request)
                        .unwrap_or(replay::BackRequest { count: Some(1) });
                    match replay.back(&id, body.count.unwrap_or(1)) {
                        Ok(state) => {
                            json_response(&serde_json::json!({ "ok": true, "replay": state }).to_string())
                        }
                        Err(e) => json_error(400, &e),
                    }
                }
                None => json_error(404, "replay disabled"),
            },
            (Method::Post, "/api/replay/config") => match &replay {
                Some(replay) => {
                    let id = query_param(&raw_url, "id").unwrap_or_default();
                    match replay::read_json(&mut request)
                        .and_then(|body| replay.update_config(&id, body))
                    {
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
            (Method::Get, "/api/annotations")
            | (Method::Post, "/api/annotations")
            | (Method::Delete, "/api/annotations") => {
                let run_id = query_param(&raw_url, "id").unwrap_or_default();
                let ann_id = query_param(&raw_url, "ann").unwrap_or_default();
                handle_annotations(
                    &annotation_config,
                    &run_id,
                    &ann_id,
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

/// One window of a paged `/api/result` response. Borrows straight from the cached
/// `BacktestResult` so serialization is a single pass to the output string.
#[derive(Serialize)]
struct ResultPage<'a> {
    total_fps: usize,
    total_eq: usize,
    footprints_from: usize,
    footprints: &'a [Footprint],
    equity_from: usize,
    equity: &'a [EquityPoint],
    /// Every non-paged field of the result; present only on the first page.
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<ResultMeta<'a>>,
}

#[derive(Serialize)]
struct ResultMeta<'a> {
    metrics: &'a Metrics,
    trades: &'a [Trade],
    series: &'a [Series],
    liquidity_heatmap: &'a LiquidityHeatmap,
    tick_size: f64,
    multiplier: f64,
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
    // loadResult() reads globalThis.RUN_ID directly (empty id falls back to the first
    // run server-side), so injecting RUN_ID is all the per-run wiring the page needs.
    STUDIO_HTML.replace(
        "const price=$(\"price\"), pctx=price.getContext(\"2d\");",
        &format!(
            "globalThis.RUN_ID={escaped};\nglobalThis.RUN_STRATEGY={escaped_strategy};\nglobalThis.RUN_SUMMARY={summary_json};\nglobalThis.ANNOTATION_CONFIG={annotation_json};\nconst price=$(\"price\"), pctx=price.getContext(\"2d\");"
        ),
    )
}

fn handle_annotations(
    cfg: &AnnotationConfig,
    run_id: &str,
    ann_id: &str,
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
        Method::Delete => match delete_annotation(cfg, run_id, ann_id) {
            Ok(true) => json_response(&serde_json::json!({ "ok": true }).to_string()),
            Ok(false) => json_error(404, "annotation not found"),
            Err(e) => json_error(400, &e),
        },
        _ => Response::from_string("method not allowed").with_status_code(405),
    }
}

/// Remove one annotation (matched by its `id` and `run`) from the JSONL file, rewriting it
/// atomically. Returns Ok(false) when nothing matched.
fn delete_annotation(cfg: &AnnotationConfig, run_id: &str, ann_id: &str) -> Result<bool, String> {
    if ann_id.is_empty() {
        return Err("ann query parameter is required".to_string());
    }
    let path = Path::new(&cfg.save_path);
    if !path.exists() {
        return Ok(false);
    }
    let file = File::open(path).map_err(|e| format!("open annotation file: {e}"))?;
    let mut kept = Vec::new();
    let mut removed = false;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("read annotation file: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let matches = serde_json::from_str::<Value>(&line).is_ok_and(|value| {
            value.get("id").and_then(Value::as_str) == Some(ann_id)
                && (run_id.is_empty()
                    || value.get("run").and_then(Value::as_str) == Some(run_id))
        });
        if matches && !removed {
            removed = true;
        } else {
            kept.push(line);
        }
    }
    if !removed {
        return Ok(false);
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut out = File::create(&tmp).map_err(|e| format!("create annotation tmp: {e}"))?;
        for line in &kept {
            writeln!(out, "{line}").map_err(|e| format!("write annotation tmp: {e}"))?;
        }
    }
    fs::rename(&tmp, path).map_err(|e| format!("replace annotation file: {e}"))?;
    Ok(true)
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

fn query_usize(url: &str, key: &str) -> Option<usize> {
    query_param(url, key)?.parse().ok()
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
