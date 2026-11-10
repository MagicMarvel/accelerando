//! Batteries-included runner for Accelerando.
//!
//! A complete strategy project is a strategy file plus:
//!
//! ```ignore
//! fn main() {
//!     accelerando_app::App::new()
//!         .strategy::<MyStrategy>("my_strategy")
//!         .run_cli();
//! }
//! ```
//!
//! and then:
//!
//! ```text
//! cargo run --release -- data.csv --strategy my_strategy --param stop_ticks=8 --serve
//! cargo run --release -- data.csv --strategy my_strategy --grid --validate
//! cargo run --release -- --list
//! ```
//!
//! What the runner does for you:
//!
//! * builds source → aggregator → the indicators the strategy declares in
//!   [`Strategy::requirements`] (plus any `--indicator`) → strategy;
//! * caches prepared footprints on disk keyed by file identity + pipeline config;
//! * checks that every value key the strategy reads is actually published;
//! * runs a single parameter set or the `ParamSpec` grid in parallel and prints a summary table;
//! * `--validate`: IS/OOS split, walk-forward, cost sensitivity and a random-entry baseline;
//! * `--serve`: opens the web viewer on the (best) run; `--trades-json` / `--result-json` dump.
//!
//! Everything is also available as functions ([`App::prepare`], [`App::run_candidates`],
//! [`validate`]) for runners that need custom flows.

pub mod stats;
pub mod validate;

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use accelerando_core::{
    prepare_backtest_data, run_prepared_backtest, run_prepared_event_backtest, BacktestResult,
    BrokerConfig, Configurable, DataSource, EventInterest, FootprintAggregator, Indicator,
    IndicatorRequest, ParamRange, ParamValue, Params, PreparedBacktestData, ProgressHandle,
    Registry, Strategy, StrategyRequirements,
};
use rayon::prelude::*;

pub use stats::TradeStats;
pub use validate::{Candidate, ValidationReport};

/// Bumped whenever the cached `PreparedBacktestData` layout or preparation semantics change.
const CACHE_VERSION: u32 = 1;

/// Runner options. Every field maps to a CLI flag; construct directly for programmatic use.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub data: Option<String>,
    pub strategy: Option<String>,
    pub params: Vec<String>,
    pub grid: bool,
    pub max_evals: usize,
    /// Aggregator spec: `time:60`, `range:30`, `reverse:16` or `name:key=value,key=value`.
    pub agg: String,
    pub source: String,
    pub buy_aggressor_code: i64,
    /// Extra indicators: `name` or `name:key=value,key=value`.
    pub indicators: Vec<String>,
    pub commission: f64,
    pub slippage_ticks: f64,
    pub starting_equity: f64,
    pub jobs: usize,
    pub cache_dir: Option<PathBuf>,
    pub validate: bool,
    pub split: f64,
    pub folds: usize,
    pub random_runs: usize,
    pub validate_json: Option<PathBuf>,
    pub trades_json: Option<PathBuf>,
    pub result_json: Option<PathBuf>,
    pub serve: bool,
    pub port: u16,
    pub list: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            data: None,
            strategy: None,
            params: Vec::new(),
            grid: false,
            max_evals: 256,
            agg: "time:60".into(),
            source: "csv".into(),
            buy_aggressor_code: 1,
            indicators: Vec::new(),
            commission: 2.0,
            slippage_ticks: 1.0,
            starting_equity: 100_000.0,
            jobs: 0,
            cache_dir: Some(PathBuf::from("prepared_cache")),
            validate: false,
            split: 0.7,
            folds: 4,
            random_runs: 50,
            validate_json: None,
            trades_json: None,
            result_json: None,
            serve: false,
            port: 8080,
            list: false,
        }
    }
}

const HELP: &str = "\
usage: <runner> <data.csv> --strategy NAME [options]

strategy & parameters
  --strategy NAME            registered strategy to run (see --list)
  --param name=value         pin one parameter (repeatable; bools accept true/false)
  --grid                     expand every unpinned parameter over its ParamSpec range
  --max-evals N              cap the grid (default 256)
  --list                     list strategies, indicators and their parameters

data & pipeline
  --agg SPEC                 time:60 | range:30 | reverse:16 | name:k=v,k=v  (default time:60)
  --source NAME              data source adapter (default csv)
  --buy-aggressor-code N     CSV side code meaning buy aggressor (default 1 = Bookmap)
  --indicator SPEC           extra indicator, name or name:k=v,k=v (repeatable)
  --no-cache / --cache-dir D prepared-footprint disk cache (default ./prepared_cache)

broker
  --commission X             per contract per side (default 2.0)
  --slippage-ticks X         per side (default 1)
  --equity X                 starting equity (default 100000)

validation
  --validate                 IS/OOS split, walk-forward (with --grid), cost sensitivity,
                             random-entry baseline
  --split F                  IS fraction of the time span (default 0.7)
  --folds K                  walk-forward folds (default 4)
  --random-runs N            random baseline runs (default 50)
  --validate-json PATH       write the validation report

output
  --trades-json PATH         all runs' trades
  --result-json PATH         full BacktestResult of the best run
  --serve [--port P]         open the web viewer on the best run (default port 8080)
  --jobs N                   worker threads (default: all cores)
";

impl RunOptions {
    /// Parse CLI arguments (without the program name).
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = RunOptions::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            let num = |name: &str, raw: String| -> Result<f64, String> {
                raw.parse::<f64>()
                    .map_err(|_| format!("{name}: `{raw}` is not a number"))
            };
            match arg.as_str() {
                "-h" | "--help" => return Err(HELP.to_string()),
                "--strategy" => o.strategy = Some(value(arg)?),
                "--param" => o.params.push(value(arg)?),
                "--grid" => o.grid = true,
                "--no-grid" => o.grid = false,
                "--max-evals" => o.max_evals = num(arg, value(arg)?)? as usize,
                "--agg" => o.agg = value(arg)?,
                "--source" => o.source = value(arg)?,
                "--buy-aggressor-code" => o.buy_aggressor_code = num(arg, value(arg)?)? as i64,
                "--indicator" => o.indicators.push(value(arg)?),
                "--commission" => o.commission = num(arg, value(arg)?)?,
                "--slippage-ticks" => o.slippage_ticks = num(arg, value(arg)?)?,
                "--equity" => o.starting_equity = num(arg, value(arg)?)?,
                "--jobs" => o.jobs = num(arg, value(arg)?)? as usize,
                "--no-cache" => o.cache_dir = None,
                "--cache-dir" => o.cache_dir = Some(PathBuf::from(value(arg)?)),
                "--validate" => o.validate = true,
                "--split" => o.split = num(arg, value(arg)?)?,
                "--folds" => o.folds = num(arg, value(arg)?)? as usize,
                "--random-runs" => o.random_runs = num(arg, value(arg)?)? as usize,
                "--validate-json" => o.validate_json = Some(PathBuf::from(value(arg)?)),
                "--trades-json" => o.trades_json = Some(PathBuf::from(value(arg)?)),
                "--result-json" => o.result_json = Some(PathBuf::from(value(arg)?)),
                "--serve" => o.serve = true,
                "--port" => o.port = num(arg, value(arg)?)? as u16,
                "--list" => o.list = true,
                flag if flag.starts_with("--") => {
                    return Err(format!("unknown flag `{flag}`\n\n{HELP}"))
                }
                path => {
                    if o.data.replace(path.to_string()).is_some() {
                        return Err(format!("more than one data path given (`{path}`)"));
                    }
                }
            }
        }
        Ok(o)
    }

    pub fn broker(&self) -> BrokerConfig {
        BrokerConfig {
            commission_per_contract: self.commission,
            slippage_ticks: self.slippage_ticks,
            starting_equity: self.starting_equity,
        }
    }
}

/// Split `name:rest` where rest is either a bare number (shorthand) or `k=v,k=v`.
fn parse_component_spec(spec: &str, shorthand: &[(&str, &str)]) -> (String, Vec<String>) {
    let (name, rest) = spec.split_once(':').unwrap_or((spec, ""));
    let rest = rest.trim();
    let assignments = if rest.is_empty() {
        Vec::new()
    } else if !rest.contains('=') {
        match shorthand.iter().find(|(n, _)| *n == name) {
            Some((_, key)) => vec![format!("{key}={rest}")],
            None => vec![rest.to_string()],
        }
    } else {
        rest.split(',').map(|s| s.trim().to_string()).collect()
    };
    (name.to_string(), assignments)
}

const AGG_SHORTHAND: &[(&str, &str)] = &[
    ("time", "bar_secs"),
    ("range", "range_ticks"),
    ("reverse", "reversal_ticks"),
];

/// Prepared data plus how it was built.
pub struct Prepared {
    pub data: PreparedBacktestData,
    pub indicators: Vec<IndicatorRequest>,
    pub from_cache: bool,
}

/// One finished run.
pub struct RunOutcome {
    pub label: String,
    pub params: Params,
    pub result: BacktestResult,
}

/// The embeddable runner: a registry plus the CLI / batch logic around it.
pub struct App {
    registry: Registry,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Start from the built-in adapters (`accelerando::default_registry()`).
    pub fn new() -> Self {
        Self {
            registry: accelerando::default_registry(),
        }
    }

    /// Start from your own registry.
    pub fn with_registry(registry: Registry) -> Self {
        Self { registry }
    }

    pub fn strategy<T: Strategy + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry.register_strategy::<T>(name);
        self
    }

    pub fn indicator<T: Indicator + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry.register_indicator::<T>(name);
        self
    }

    pub fn aggregator<T: FootprintAggregator + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry.register_aggregator::<T>(name);
        self
    }

    pub fn source<T: DataSource + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry.register_source::<T>(name);
        self
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// Parse `std::env::args`, run, and exit non-zero on error.
    pub fn run_cli(self) {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let code = match RunOptions::parse(&args).and_then(|o| self.run(&o)) {
            Ok(()) => 0,
            Err(msg) => {
                eprintln!("{msg}");
                2
            }
        };
        std::process::exit(code);
    }

    /// Run with explicit options.
    pub fn run(&self, o: &RunOptions) -> Result<(), String> {
        if o.list {
            self.print_list();
            return Ok(());
        }
        if o.jobs > 0 {
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(o.jobs)
                .build_global();
        }
        let strategy = o.strategy.as_deref().ok_or_else(|| {
            format!(
                "missing --strategy NAME (registered: {})\n\n{HELP}",
                self.registry.strategy_names().join(", ")
            )
        })?;
        let data_path = o
            .data
            .as_deref()
            .ok_or_else(|| format!("missing data path\n\n{HELP}"))?;
        let candidates =
            accelerando::strategy_param_grid(&self.registry, strategy, &o.params, o.max_evals, o.grid)?;
        let probe = self.build_strategy(strategy, &candidates[0])?;
        let reqs = probe.requirements();
        let event_driven = probe.event_interest() != EventInterest::NONE;
        drop(probe);
        if let Some(bars) = &reqs.bars {
            if bars != &o.agg {
                eprintln!(
                    "WARNING: `{strategy}` was designed for --agg {bars}; running on {} (results are not comparable)",
                    o.agg
                );
            }
        }
        println!(
            "strategy {strategy}: {} parameter set(s){}",
            candidates.len(),
            if o.grid { " (grid)" } else { "" }
        );
        if o.grid {
            let full = accelerando::strategy_grid_size(&self.registry, strategy, &o.params)?;
            if full > candidates.len() as u128 {
                println!(
                    "note: full grid has {full} points; sampled {} evenly. Pin parameters with --param or raise --max-evals.",
                    candidates.len()
                );
            }
        }
        if candidates.len() == 1 {
            println!("params: {}", fmt_params(&candidates[0]));
        }

        let started = Instant::now();
        let outcomes = if event_driven {
            if o.validate {
                eprintln!("note: --validate covers footprint strategies only; skipping for event-driven `{strategy}`");
            }
            self.run_event(o, data_path, strategy, &reqs, &candidates)?
        } else {
            let prepared = self.prepare(o, data_path, &reqs)?;
            prepared.data.check_values(&reqs.values)?;
            let keep = o.serve && candidates.len() == 1;
            let outcomes = self.run_candidates(&prepared.data, strategy, candidates, o.broker(), keep)?;
            print_table(&outcomes);
            if o.validate {
                let report = self.validate(o, &prepared.data, strategy, &outcomes)?;
                report.print();
                if let Some(path) = &o.validate_json {
                    write_json(path, &report)?;
                }
            }
            outcomes
        };
        if event_driven {
            print_table(&outcomes);
        }
        println!("elapsed {:.1}s", started.elapsed().as_secs_f64());

        let best = outcomes
            .iter()
            .max_by(|a, b| a.result.metrics.net_pnl.total_cmp(&b.result.metrics.net_pnl))
            .ok_or("no runs produced")?;
        if let Some(path) = &o.trades_json {
            let dump: Vec<_> = outcomes
                .iter()
                .map(|r| serde_json::json!({"label": r.label, "params": r.params, "metrics": r.result.metrics, "trades": r.result.trades}))
                .collect();
            write_json(path, &dump)?;
        }
        if let Some(path) = &o.result_json {
            write_json(path, &best.result)?;
        }
        if o.serve {
            let result = if best.result.footprints.is_empty() {
                // Grid runs drop footprints for memory; rerun the best one for the chart.
                println!("re-running {} with chart data ...", best.label);
                let prepared = self.prepare(o, data_path, &reqs)?;
                run_prepared_backtest(
                    &prepared.data,
                    self.build_strategy(strategy, &best.params)?,
                    o.broker(),
                    true,
                )
            } else {
                best.result.clone()
            };
            accelerando_web::serve(&result, o.port).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn build_strategy(&self, name: &str, params: &Params) -> Result<Box<dyn Strategy>, String> {
        self.registry
            .build_strategy(name, params)
            .ok_or_else(|| format!("strategy `{name}` not registered"))
    }

    /// Requested indicators = strategy requirements + `--indicator` extras (deduplicated).
    fn indicator_requests(
        &self,
        o: &RunOptions,
        reqs: &StrategyRequirements,
    ) -> Result<Vec<IndicatorRequest>, String> {
        let mut out = reqs.indicators.clone();
        for spec in &o.indicators {
            let (name, assignments) = parse_component_spec(spec, &[]);
            let ispec = self.registry.indicator_spec(&name).ok_or_else(|| {
                format!(
                    "unknown indicator `{name}`; registered: {}",
                    self.registry.indicator_names().join(", ")
                )
            })?;
            let params = ispec.parse_assignments(&assignments)?;
            out.push(IndicatorRequest { name, params });
        }
        let mut seen = Vec::new();
        out.retain(|r| {
            let dup = seen.contains(r);
            if !dup {
                seen.push(r.clone());
            }
            !dup
        });
        Ok(out)
    }

    fn build_source(&self, o: &RunOptions, path: &str) -> Result<Box<dyn DataSource>, String> {
        std::fs::metadata(path).map_err(|e| format!("cannot read data file {path}: {e}"))?;
        let spec = self
            .registry
            .source_spec(&o.source)
            .ok_or_else(|| format!("unknown source `{}`", o.source))?;
        let mut params = Params::default().with("path", path);
        if spec.entries.iter().any(|e| e.name == "buy_aggressor_code") {
            params.set("buy_aggressor_code", ParamValue::Int(o.buy_aggressor_code));
        }
        self.registry
            .build_source(&o.source, &params)
            .ok_or_else(|| format!("source `{}` not registered", o.source))
    }

    fn build_aggregator(&self, agg: &str) -> Result<Box<dyn FootprintAggregator>, String> {
        let (name, assignments) = parse_component_spec(agg, AGG_SHORTHAND);
        let spec = self.registry.aggregator_spec(&name).ok_or_else(|| {
            format!(
                "unknown aggregator `{name}`; registered: {}",
                self.registry.aggregator_names().join(", ")
            )
        })?;
        let params = spec.parse_assignments(&assignments)?;
        self.registry
            .build_aggregator(&name, &params)
            .ok_or_else(|| format!("aggregator `{name}` not registered"))
    }

    fn build_indicators(&self, requests: &[IndicatorRequest]) -> Result<Vec<Box<dyn Indicator>>, String> {
        requests
            .iter()
            .map(|r| {
                let spec = self.registry.indicator_spec(&r.name).ok_or_else(|| {
                    format!(
                        "strategy requires indicator `{}`, which is not registered; registered: {}",
                        r.name,
                        self.registry.indicator_names().join(", ")
                    )
                })?;
                spec.validate(&r.params)
                    .map_err(|e| format!("indicator `{}`: {e}", r.name))?;
                Ok(self.registry.build_indicator(&r.name, &r.params).expect("checked"))
            })
            .collect()
    }

    /// Prepare (or load from cache) enriched footprints for `reqs`.
    pub fn prepare(
        &self,
        o: &RunOptions,
        path: &str,
        reqs: &StrategyRequirements,
    ) -> Result<Prepared, String> {
        let indicators = self.indicator_requests(o, reqs)?;
        let cache_path = o
            .cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("prep_{:016x}.json", cache_key(o, path, &indicators))));
        if let Some(cp) = &cache_path {
            if let Ok(bytes) = std::fs::read(cp) {
                match serde_json::from_slice::<PreparedBacktestData>(&bytes) {
                    Ok(data) => {
                        println!("prepared-data cache hit: {}", cp.display());
                        return Ok(Prepared {
                            data,
                            indicators,
                            from_cache: true,
                        });
                    }
                    Err(e) => eprintln!("cache {} unreadable ({e}); re-preparing", cp.display()),
                }
            }
        }
        let mut source = self.build_source(o, path)?;
        let progress = ProgressHandle::new();
        source.set_progress(progress.clone());
        let reporter = spawn_progress(progress.clone());
        let data = prepare_backtest_data(
            source,
            self.build_aggregator(&o.agg)?,
            self.build_indicators(&indicators)?,
            false,
            Some(progress),
        );
        let _ = reporter.join();
        println!(
            "prepared {} footprints (tick {}, multiplier {})",
            data.footprints.len(),
            data.tick_size,
            data.multiplier
        );
        if let Some(cp) = &cache_path {
            if let Ok(bytes) = serde_json::to_vec(&data) {
                if let Some(dir) = cp.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let tmp = cp.with_extension("json.tmp");
                if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, cp).is_ok() {
                    println!("prepared-data cache written: {}", cp.display());
                }
            }
        }
        Ok(Prepared {
            data,
            indicators,
            from_cache: false,
        })
    }

    /// Run every parameter set over shared prepared data, in parallel.
    pub fn run_candidates(
        &self,
        data: &PreparedBacktestData,
        strategy: &str,
        candidates: Vec<Params>,
        cfg: BrokerConfig,
        keep_footprints: bool,
    ) -> Result<Vec<RunOutcome>, String> {
        let total = candidates.len();
        let done = AtomicUsize::new(0);
        let labels = labels(strategy, &candidates);
        let out: Vec<RunOutcome> = candidates
            .into_par_iter()
            .zip(labels)
            .map(|(params, label)| {
                let s = self
                    .registry
                    .build_strategy(strategy, &params)
                    .expect("strategy registered");
                let result = run_prepared_backtest(data, s, cfg, keep_footprints);
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if total > 1 {
                    print!("\rbacktests {n}/{total}");
                    let _ = std::io::stdout().flush();
                }
                RunOutcome {
                    label,
                    params,
                    result,
                }
            })
            .collect();
        if total > 1 {
            println!();
        }
        Ok(out)
    }

    fn run_event(
        &self,
        o: &RunOptions,
        path: &str,
        strategy: &str,
        reqs: &StrategyRequirements,
        candidates: &[Params],
    ) -> Result<Vec<RunOutcome>, String> {
        let events = accelerando_core::prepare_event_data(self.build_source(o, path)?)?;
        let requests = self.indicator_requests(o, reqs)?;
        let labels = labels(strategy, candidates);
        candidates
            .iter()
            .zip(labels)
            .map(|(params, label)| {
                let result = run_prepared_event_backtest(
                    &events,
                    self.build_aggregator(&o.agg)?,
                    self.build_indicators(&requests)?,
                    self.build_strategy(strategy, params)?,
                    o.broker(),
                    o.serve && candidates.len() == 1,
                    None,
                )?;
                Ok(RunOutcome {
                    label,
                    params: params.clone(),
                    result,
                })
            })
            .collect()
    }

    /// The `--validate` report for already-run outcomes.
    pub fn validate(
        &self,
        o: &RunOptions,
        data: &PreparedBacktestData,
        strategy: &str,
        outcomes: &[RunOutcome],
    ) -> Result<ValidationReport, String> {
        println!("running validation ...");
        let candidates: Vec<Candidate> = outcomes
            .iter()
            .map(|r| Candidate {
                label: r.label.clone(),
                params: r.params.clone(),
                result: BacktestResult {
                    footprints: Vec::new(),
                    ..r.result.clone()
                },
            })
            .collect();
        let split = validate::split_report(data, &candidates, o.split);
        let walk_forward = validate::walk_forward(data, &candidates, o.folds, 5);
        // Cost/random checks use the IS winner when a grid ran (never the full-sample winner).
        let chosen = split
            .as_ref()
            .and_then(|s| candidates.iter().find(|c| c.label == s.chosen))
            .unwrap_or(&candidates[0]);
        let params = chosen.params.clone();
        let build = || {
            self.registry
                .build_strategy(strategy, &params)
                .expect("strategy registered")
        };
        let costs = validate::cost_sensitivity(data, &build, o.broker(), &[0.0, 1.0, 2.0, 3.0]);
        let random = validate::random_baseline(data, &chosen.result.trades, o.broker(), o.random_runs, 42);
        Ok(ValidationReport {
            split,
            walk_forward,
            costs,
            random,
        })
    }

    fn print_list(&self) {
        let print_spec = |spec: accelerando_core::ParamSpec| {
            for e in spec.entries {
                let range = match e.range {
                    ParamRange::Int { min, max, step } => format!("int {min}..={max} step {step}"),
                    ParamRange::Float { min, max, step } => match step {
                        Some(s) => format!("float {min}..={max} step {s}"),
                        None => format!("float {min}..={max}"),
                    },
                    ParamRange::Choice(opts) => format!("one of {}", opts.join("|")),
                    ParamRange::Fixed => "fixed".to_string(),
                };
                println!("    {:<32} default {:<14} {range}", e.name, fmt_value(&e.default));
            }
        };
        println!("strategies:");
        for name in self.registry.strategy_names() {
            println!("  {name}");
            if let Some(spec) = self.registry.strategy_spec(name) {
                print_spec(spec);
            }
        }
        println!("\nindicators:");
        for name in self.registry.indicator_names() {
            println!("  {name}");
        }
        println!("\naggregators: {}", self.registry.aggregator_names().join(", "));
        println!("sources: {}", self.registry.source_names().join(", "));
    }
}

fn labels(strategy: &str, candidates: &[Params]) -> Vec<String> {
    if candidates.len() == 1 {
        return vec![strategy.to_string()];
    }
    (0..candidates.len())
        .map(|i| format!("{strategy}#{:04}", i + 1))
        .collect()
}

fn fmt_value(v: &ParamValue) -> String {
    match v {
        ParamValue::Int(i) => i.to_string(),
        ParamValue::Float(f) => f.to_string(),
        ParamValue::Str(s) => s.clone(),
    }
}

fn fmt_params(p: &Params) -> String {
    p.0.iter()
        .map(|(k, v)| format!("{k}={}", fmt_value(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_table(outcomes: &[RunOutcome]) {
    let mut rows: Vec<&RunOutcome> = outcomes.iter().collect();
    rows.sort_by(|a, b| b.result.metrics.net_pnl.total_cmp(&a.result.metrics.net_pnl));
    println!(
        "\n{:<24} {:>11} {:>7} {:>7} {:>6} {:>10} {:>7}",
        "run", "net", "trades", "win%", "PF", "maxDD", "sharpe"
    );
    for r in rows.iter().take(20) {
        let m = &r.result.metrics;
        println!(
            "{:<24} {:>11.2} {:>7} {:>6.1}% {:>6} {:>10.2} {:>7.2}",
            r.label,
            m.net_pnl,
            m.trades,
            m.win_rate * 100.0,
            stats::fmt_pf(m.profit_factor),
            m.max_drawdown.abs(),
            m.sharpe
        );
    }
    if rows.len() > 20 {
        println!("... {} more (use --trades-json for all)", rows.len() - 20);
        if let Some(best) = rows.first() {
            println!("best: {} {}", best.label, fmt_params(&best.params));
        }
    }
}

fn cache_key(o: &RunOptions, path: &str, indicators: &[IndicatorRequest]) -> u64 {
    let meta = std::fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let key = serde_json::json!({
        "v": CACHE_VERSION,
        "path": std::fs::canonicalize(path).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string()),
        "size": meta.map(|m| m.len()).unwrap_or(0),
        "mtime": mtime.to_string(),
        "source": o.source,
        "buy_aggressor_code": o.buy_aggressor_code,
        "agg": o.agg,
        "indicators": indicators,
    });
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.to_string().hash(&mut h);
    h.finish()
}

fn write_json<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn spawn_progress(progress: ProgressHandle) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        let s = progress.snapshot();
        match s.fraction() {
            Some(f) => print!(
                "\rpreparing {:>5.1}%  {}/{} MB  footprints {}",
                f * 100.0,
                s.bytes / 1_000_000,
                s.total_bytes / 1_000_000,
                s.footprints
            ),
            None => print!("\rpreparing footprints {}", s.footprints),
        }
        let _ = std::io::stdout().flush();
        if s.done {
            println!();
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_specs() {
        assert_eq!(
            parse_component_spec("time:60", AGG_SHORTHAND),
            ("time".into(), vec!["bar_secs=60".into()])
        );
        assert_eq!(
            parse_component_spec("range:range_ticks=20", AGG_SHORTHAND),
            ("range".into(), vec!["range_ticks=20".into()])
        );
        assert_eq!(parse_component_spec("regime", &[]), ("regime".into(), vec![]));
    }

    #[test]
    fn cli_parses_and_rejects_unknown_flags() {
        let args: Vec<String> = ["data.csv", "--strategy", "x", "--param", "a=1", "--grid", "--validate"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = RunOptions::parse(&args).unwrap();
        assert_eq!(o.data.as_deref(), Some("data.csv"));
        assert!(o.grid && o.validate);
        assert_eq!(o.params, vec!["a=1".to_string()]);
        assert!(RunOptions::parse(&["--bogus".to_string()]).is_err());
    }
}
