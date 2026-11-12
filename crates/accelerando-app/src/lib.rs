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
//! cargo run --release -- data.csv --param stop_ticks=8 --gui
//! cargo run --release -- data.csv --strategy my_strategy --grid --validate
//! cargo run --release -- --list
//! ```
//!
//! With one registered strategy `--strategy` may be left out; without `--agg` the bars the
//! strategy declares in `requirements().bars(..)` are used. Put the flags you always pass in an
//! `accelerando.toml` next to `Cargo.toml` (see [`config`]) and a run is `cargo run --release`.
//!
//! What the runner does for you:
//!
//! * builds source → aggregator → the indicators the strategy declares in
//!   [`Strategy::requirements`] (plus any `--indicator`) → strategy;
//! * caches prepared footprints on disk keyed by file identity + pipeline config;
//! * checks that every value key the strategy reads is actually published;
//! * runs a single parameter set or the `ParamSpec` grid in parallel and prints a summary table;
//! * `--validate`: IS/OOS split, walk-forward, cost sensitivity and a random-entry baseline;
//! * `--serve` / `--gui`: opens the web / native viewer on the (best) run;
//!   `--trades-json` / `--result-json` dump.
//!
//! Extending it without forking: [`App::mode`] adds a `--mode NAME` of your own (feature
//! exports, factor mining, ...) that gets the same data/cache/strategy plumbing through
//! [`ModeContext`]; [`App::with_indicator`] adds an indicator to every preparation.
//! Everything is also available as functions ([`App::prepare`], [`App::run_candidates`],
//! [`validate`]) for runners that need custom flows.

pub mod config;
mod events;
mod gui;
pub mod stats;
pub mod validate;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use accelerando_core::{
    prepare_backtest_data, run_prepared_backtest, run_prepared_event_backtest, BacktestResult,
    BrokerConfig, Configurable, DataSource, EventInterest, FootprintAggregator, Indicator,
    IndicatorRequest, ParamRange, ParamValue, Params, PreparedBacktestData, PreparedEventData,
    ProgressHandle, Registry, Strategy, StrategyRequirements,
};
use rayon::prelude::*;

pub use config::CONFIG_FILE;
pub use stats::TradeStats;
pub use validate::{Candidate, ValidationReport};

/// Bumped whenever the cached `PreparedBacktestData` layout or preparation semantics change.
const CACHE_VERSION: u32 = 1;

/// Bars used when neither `--agg` nor the strategy's `requirements().bars` says otherwise.
pub const DEFAULT_AGG: &str = "time:60";

/// Runner options. Every field maps to a CLI flag; construct directly for programmatic use.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub data: Option<String>,
    pub strategy: Option<String>,
    pub params: Vec<String>,
    pub grid: bool,
    pub max_evals: usize,
    /// Aggregator spec: `time:60`, `range:30`, `reverse:16` or `name:key=value,key=value`.
    /// `--agg`; `None` = the strategy's declared bars, else [`DEFAULT_AGG`]. Read with
    /// [`RunOptions::agg`].
    pub agg: Option<String>,
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
    pub gui: bool,
    /// `--mode NAME`: run a mode registered with [`App::mode`] instead of a backtest.
    pub mode: Option<String>,
    /// `--opt key=value` (repeatable): free-form options for the selected mode.
    pub opts: Vec<String>,
    /// `--config PATH` instead of `./accelerando.toml`.
    pub config: Option<PathBuf>,
    /// `--no-config`: ignore `accelerando.toml`.
    pub no_config: bool,
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
            agg: None,
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
            gui: false,
            mode: None,
            opts: Vec::new(),
            config: None,
            no_config: false,
            port: 8080,
            list: false,
        }
    }
}

const HELP: &str = "\
usage: <runner> <data.csv> --strategy NAME [options]

strategy & parameters
  --strategy NAME            strategy to run (see --list); optional when the app registers one
  --param name=value         pin one parameter (repeatable; bools accept true/false)
  --grid                     expand every unpinned parameter over its ParamSpec range
  --max-evals N              cap the grid (default 256)
  --list                     list strategies, indicators and their parameters

data & pipeline
  --agg SPEC                 time:60 | range:30 | reverse:16 | name:k=v,k=v
                             (default: the strategy's declared bars, else time:60)
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
  --gui / --no-gui           open the native viewer on the best run (bar-type switch, tick replay)
  --jobs N                   worker threads (default: all cores)

project
  --config PATH              read defaults from PATH instead of ./accelerando.toml
  --no-config                ignore accelerando.toml
  --mode NAME [--opt k=v]    run a custom mode registered by the app (see --list)
";

impl RunOptions {
    /// Parse CLI arguments (without the program name).
    pub fn parse(args: &[String]) -> Result<Self, String> {
        Self::parse_onto(RunOptions::default(), args)
    }

    /// Parse `args` on top of `base` (e.g. the options from `accelerando.toml`): flags given in
    /// `args` replace `base`'s, a `--param x=..` replaces `base`'s `x`, and a different
    /// `--strategy` drops `base`'s params.
    pub fn parse_onto(base: RunOptions, args: &[String]) -> Result<Self, String> {
        let base_strategy = base.strategy.clone();
        let base_params = base.params.clone();
        let mut o = RunOptions {
            data: None,
            params: Vec::new(),
            ..base.clone()
        };
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
                "--agg" => o.agg = Some(value(arg)?),
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
                "--gui" => o.gui = true,
                "--no-gui" => o.gui = false,
                "--mode" => o.mode = Some(value(arg)?),
                "--opt" => o.opts.push(value(arg)?),
                "--config" => o.config = Some(PathBuf::from(value(arg)?)),
                "--no-config" => o.no_config = true,
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
        if o.data.is_none() {
            o.data = base.data;
        }
        if o.strategy == base_strategy {
            let pinned: Vec<&str> = o.params.iter().filter_map(|p| p.split_once('=').map(|(k, _)| k.trim())).collect();
            let mut merged: Vec<String> = base_params
                .into_iter()
                .filter(|p| !p.split_once('=').is_some_and(|(k, _)| pinned.contains(&k.trim())))
                .collect();
            merged.append(&mut o.params);
            o.params = merged;
        }
        if !o.opts.is_empty() && o.mode.is_none() {
            return Err("--opt only applies to a --mode".into());
        }
        Ok(o)
    }

    /// The bar spec in effect (`--agg`, else [`DEFAULT_AGG`]; `App::run` fills in the
    /// strategy's declared bars first).
    pub fn agg(&self) -> &str {
        self.agg.as_deref().unwrap_or(DEFAULT_AGG)
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

/// A custom `--mode` handler (see [`App::mode`]).
pub type ModeFn = Arc<dyn Fn(&ModeContext) -> Result<(), String> + Send + Sync>;

struct Mode {
    about: String,
    run: ModeFn,
}

/// The embeddable runner: a registry plus the CLI / batch logic around it.
pub struct App {
    registry: Arc<Registry>,
    modes: BTreeMap<String, Mode>,
    base_indicators: Vec<IndicatorRequest>,
    /// Strategies registered through [`App::strategy`] (not the built-in examples).
    own_strategies: Vec<String>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Start from the built-in adapters (`accelerando::default_registry()`).
    pub fn new() -> Self {
        Self::with_registry(accelerando::default_registry())
    }

    /// Start from your own registry.
    pub fn with_registry(registry: Registry) -> Self {
        Self {
            registry: Arc::new(registry),
            modes: BTreeMap::new(),
            base_indicators: Vec::new(),
            own_strategies: Vec::new(),
        }
    }

    pub fn strategy<T: Strategy + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry_mut().register_strategy::<T>(name);
        self.own_strategies.retain(|n| n != name);
        self.own_strategies.push(name.to_string());
        self
    }

    pub fn indicator<T: Indicator + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry_mut().register_indicator::<T>(name);
        self
    }

    pub fn aggregator<T: FootprintAggregator + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry_mut().register_aggregator::<T>(name);
        self
    }

    pub fn source<T: DataSource + Configurable + 'static>(mut self, name: &str) -> Self {
        self.registry_mut().register_source::<T>(name);
        self
    }

    /// Prepare this indicator for every run, on top of what the strategy requests (shared
    /// project-wide features: regime tags, big-trade markers, ...). `spec` is `name` or
    /// `name:k=v,k=v`, checked against the registered indicator when the app runs.
    pub fn with_indicator(mut self, spec: &str) -> Self {
        let (name, assignments) = parse_component_spec(spec, &[]);
        let params = self
            .registry
            .indicator_spec(&name)
            .map(|s| s.parse_assignments(&assignments).unwrap_or_else(|e| panic!("with_indicator({spec}): {e}")))
            .unwrap_or_else(|| panic!("with_indicator({spec}): indicator `{name}` is not registered (register it first)"));
        self.base_indicators.push(IndicatorRequest { name, params });
        self
    }

    /// Add `--mode NAME`: instead of the backtest, `run` calls `f` with the resolved options and
    /// the app's data plumbing (see [`ModeContext`]). Mode-specific knobs come in as
    /// `--opt key=value` and are read with [`ModeContext::opt`].
    pub fn mode<F>(mut self, name: &str, about: &str, f: F) -> Self
    where
        F: Fn(&ModeContext) -> Result<(), String> + Send + Sync + 'static,
    {
        self.modes.insert(
            name.to_string(),
            Mode {
                about: about.to_string(),
                run: Arc::new(f),
            },
        );
        self
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Mutable access while the app is being built (before any run shares the registry).
    pub fn registry_mut(&mut self) -> &mut Registry {
        Arc::get_mut(&mut self.registry).expect("registry is shared; register adapters before running")
    }

    /// Options for `args` layered over `accelerando.toml` (or `--config PATH`; `--no-config`
    /// skips it). This is what [`App::run_cli`] runs.
    pub fn options_from(&self, args: &[String]) -> Result<RunOptions, String> {
        let cli = RunOptions::parse(args)?;
        if cli.no_config {
            return Ok(cli);
        }
        let path = match &cli.config {
            Some(p) => p.clone(),
            None if Path::new(CONFIG_FILE).is_file() => PathBuf::from(CONFIG_FILE),
            None => return Ok(cli),
        };
        println!("config: {}", path.display());
        let base = RunOptions::parse(&config::args_from_file(&path)?)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        RunOptions::parse_onto(base, args)
    }

    /// Parse `std::env::args` (over `accelerando.toml`), run, and exit non-zero on error.
    pub fn run_cli(self) {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let code = match self.options_from(&args).and_then(|o| self.run(&o)) {
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
        let data_path = o
            .data
            .as_deref()
            .ok_or_else(|| format!("missing data path (give it on the command line or as `data = ...` in {CONFIG_FILE})\n\n{HELP}"))?;
        if let Some(name) = &o.mode {
            let mode = self.modes.get(name).ok_or_else(|| {
                format!(
                    "unknown --mode `{name}`; registered modes: {}",
                    if self.modes.is_empty() { "(none)".to_string() } else { self.modes.keys().cloned().collect::<Vec<_>>().join(", ") }
                )
            })?;
            return (mode.run)(&ModeContext {
                app: self,
                options: o,
                data_path,
            });
        }
        let strategy = self.strategy_name(o)?;
        let candidates =
            accelerando::strategy_param_grid(&self.registry, strategy, &o.params, o.max_evals, o.grid)?;
        let probe = self.build_strategy(strategy, &candidates[0])?;
        let reqs = probe.requirements();
        let event_driven = probe.event_interest() != EventInterest::NONE;
        drop(probe);
        let resolved = self.resolve_bars(o, strategy, &reqs);
        let o = &resolved;
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
        let mut gui_events = None;
        let outcomes = if event_driven {
            if o.validate {
                eprintln!("note: --validate covers footprint strategies only; skipping for event-driven `{strategy}`");
            }
            let source = self.build_source(o, data_path)?;
            if o.gui {
                let (events, depth) = accelerando_core::prepare_market_history(source);
                let events = Arc::new(events?);
                let outcomes = self.run_event(o, &events, strategy, &reqs, &candidates)?;
                gui_events = Some((events, depth));
                outcomes
            } else {
                let events = accelerando_core::prepare_event_data(source)?;
                self.run_event(o, &events, strategy, &reqs, &candidates)?
            }
        } else {
            let prepared = self.prepare(o, data_path, &reqs)?;
            prepared.data.check_values(&reqs.values)?;
            let keep = (o.serve || o.gui) && candidates.len() == 1;
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
        if o.serve || o.gui {
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
            if o.gui {
                return gui::view(
                    Arc::clone(&self.registry), o, data_path, &best.label, result, gui_events,
                );
            }
            accelerando_web::serve(&result, o.port).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `--strategy`, or the only strategy registered with [`App::strategy`].
    pub fn strategy_name<'a>(&'a self, o: &'a RunOptions) -> Result<&'a str, String> {
        if let Some(name) = o.strategy.as_deref() {
            return Ok(name);
        }
        match self.own_strategies.as_slice() {
            [only] => Ok(only),
            [] => Err(format!(
                "missing --strategy NAME (registered: {})

{HELP}",
                self.registry.strategy_names().join(", ")
            )),
            names => Err(format!(
                "missing --strategy NAME (registered: {})\n\n{HELP}",
                names.join(", ")
            )),
        }
    }

    /// `o` with `agg` filled in from the strategy's declared bars when `--agg` was not given;
    /// warns when an explicit `--agg` differs from them.
    pub fn resolve_bars(&self, o: &RunOptions, strategy: &str, reqs: &StrategyRequirements) -> RunOptions {
        let mut out = o.clone();
        match (&o.agg, &reqs.bars) {
            (None, Some(bars)) => {
                println!("bars: {bars} (declared by `{strategy}`)");
                out.agg = Some(bars.clone());
            }
            (Some(agg), Some(bars)) if agg != bars => eprintln!(
                "WARNING: `{strategy}` was designed for --agg {bars}; running on {agg} (results are not comparable)"
            ),
            _ => {}
        }
        out
    }

    /// Raw trades of `path` (contract + trade events), through the binary cache in
    /// `o.cache_dir`.
    pub fn load_events(&self, o: &RunOptions, path: &str) -> Result<PreparedEventData, String> {
        events::load(&self.registry, o, path)
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
        let mut out = self.base_indicators.clone();
        out.extend(reqs.indicators.iter().cloned());
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
        build_source(&self.registry, o, path)
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
            self.build_aggregator(o.agg())?,
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
        events: &PreparedEventData,
        strategy: &str,
        reqs: &StrategyRequirements,
        candidates: &[Params],
    ) -> Result<Vec<RunOutcome>, String> {
        let requests = self.indicator_requests(o, reqs)?;
        let labels = labels(strategy, candidates);
        candidates
            .iter()
            .zip(labels)
            .map(|(params, label)| {
                let result = run_prepared_event_backtest(
                    events,
                    self.build_aggregator(o.agg())?,
                    self.build_indicators(&requests)?,
                    self.build_strategy(strategy, params)?,
                    o.broker(),
                    (o.serve || o.gui) && candidates.len() == 1,
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
        if !self.modes.is_empty() {
            println!("\nmodes (--mode NAME, options via --opt key=value):");
            for (name, mode) in &self.modes {
                println!("  {name:<20} {}", mode.about);
            }
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
    // Strategy tallies (`Strategy::counters`) for the top run.
    if let Some(best) = rows.first().filter(|b| !b.result.counters.is_empty()) {
        println!("counters ({}): {}", best.label, best.result.counters.summary());
    }
}

pub(crate) fn build_source(registry: &Registry, o: &RunOptions, path: &str) -> Result<Box<dyn DataSource>, String> {
    std::fs::metadata(path).map_err(|e| format!("cannot read data file {path}: {e}"))?;
    let spec = registry
        .source_spec(&o.source)
        .ok_or_else(|| format!("unknown source `{}`", o.source))?;
    let mut params = Params::default().with("path", path);
    if spec.entries.iter().any(|e| e.name == "buy_aggressor_code") {
        params.set("buy_aggressor_code", ParamValue::Int(o.buy_aggressor_code));
    }
    registry
        .build_source(&o.source, &params)
        .ok_or_else(|| format!("source `{}` not registered", o.source))
}

/// Path + size + mtime: changes whenever the data file does.
pub(crate) fn file_identity(path: &str) -> serde_json::Value {
    let meta = std::fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    serde_json::json!({
        "path": std::fs::canonicalize(path).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string()),
        "size": meta.map(|m| m.len()).unwrap_or(0),
        "mtime": mtime.to_string(),
    })
}

pub(crate) fn hash_json(v: &serde_json::Value) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.to_string().hash(&mut h);
    h.finish()
}

/// What a custom `--mode` handler gets: the app (registry, preparation, runs), the resolved
/// options and the data path.
pub struct ModeContext<'a> {
    pub app: &'a App,
    pub options: &'a RunOptions,
    pub data_path: &'a str,
}

impl ModeContext<'_> {
    /// `--opt key=value` value of `key`.
    pub fn opt(&self, key: &str) -> Option<&str> {
        self.options
            .opts
            .iter()
            .rev()
            .find_map(|o| o.split_once('=').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim()))
    }

    /// `--opt key=value` parsed as `T`, or `default` when absent.
    pub fn opt_or<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T, String> {
        match self.opt(key) {
            None => Ok(default),
            Some(raw) => raw
                .parse()
                .map_err(|_| format!("--opt {key}={raw}: cannot parse the value")),
        }
    }

    /// Fail on `--opt` keys this mode does not know (call it first with the keys you read).
    pub fn expect_opts(&self, known: &[&str]) -> Result<(), String> {
        for o in &self.options.opts {
            let key = o.split_once('=').map(|(k, _)| k.trim()).unwrap_or(o.as_str());
            if !known.contains(&key) {
                return Err(format!("unknown --opt `{key}` for this mode; known: {}", known.join(", ")));
            }
        }
        Ok(())
    }

    /// The selected strategy (`--strategy` or the only registered one), if any.
    pub fn strategy(&self) -> Option<&str> {
        self.app.strategy_name(self.options).ok()
    }

    /// Parameter sets for the selected strategy (defaults + `--param`, or the grid).
    pub fn candidates(&self) -> Result<Vec<Params>, String> {
        let strategy = self.app.strategy_name(self.options)?;
        let o = self.options;
        accelerando::strategy_param_grid(self.app.registry(), strategy, &o.params, o.max_evals, o.grid)
    }

    /// The selected strategy's requirements, or none when no strategy is selected.
    pub fn requirements(&self) -> Result<StrategyRequirements, String> {
        let Some(strategy) = self.strategy() else {
            return Ok(StrategyRequirements::default());
        };
        let params = self.candidates()?.remove(0);
        Ok(self.app.build_strategy(strategy, &params)?.requirements())
    }

    /// Options with `agg` resolved against the selected strategy's declared bars.
    pub fn resolved_options(&self) -> Result<RunOptions, String> {
        Ok(match self.strategy() {
            Some(s) => self.app.resolve_bars(self.options, s, &self.requirements()?),
            None => self.options.clone(),
        })
    }

    /// Prepared (cached) footprints with `reqs`' indicators plus the app's and `--indicator`s.
    pub fn prepare(&self, reqs: &StrategyRequirements) -> Result<Prepared, String> {
        self.app.prepare(&self.resolved_options()?, self.data_path, reqs)
    }

    /// Raw trades (binary-cached).
    pub fn events(&self) -> Result<PreparedEventData, String> {
        self.app.load_events(self.options, self.data_path)
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
        "agg": o.agg(),
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

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn cli_layers_over_config() {
        let base = RunOptions::parse(&args("file.csv --strategy s --param a=1 --param b=2 --gui --slippage-ticks 2")).unwrap();
        let o = RunOptions::parse_onto(base.clone(), &args("--param b=3 --slippage-ticks 0")).unwrap();
        assert_eq!(o.data.as_deref(), Some("file.csv"));
        assert_eq!(o.params, vec!["a=1".to_string(), "b=3".to_string()]);
        assert_eq!(o.slippage_ticks, 0.0);
        assert!(o.gui);
        let o = RunOptions::parse_onto(base.clone(), &args("other.csv --strategy t --no-gui")).unwrap();
        assert_eq!(o.data.as_deref(), Some("other.csv"));
        assert!(o.params.is_empty() && !o.gui);
        assert!(RunOptions::parse(&args("--opt x=1")).is_err());
        assert_eq!(RunOptions::default().agg(), DEFAULT_AGG);
    }

    #[derive(accelerando_core::Configurable)]
    #[allow(dead_code)]
    struct OnRange {
        #[param(default = 3, min = 1, max = 9)]
        n: u32,
    }

    impl Strategy for OnRange {
        fn requirements(&self) -> StrategyRequirements {
            StrategyRequirements::new().bars("range:30")
        }
    }

    #[test]
    fn single_strategy_and_declared_bars_are_defaults() {
        let app = App::new().strategy::<OnRange>("on_range");
        let o = RunOptions::default();
        assert_eq!(app.strategy_name(&o).unwrap(), "on_range");
        let reqs = OnRange { n: 3 }.requirements();
        assert_eq!(app.resolve_bars(&o, "on_range", &reqs).agg(), "range:30");
        // An explicit --agg wins (with a warning).
        let explicit = RunOptions::parse(&args("--agg time:15")).unwrap();
        assert_eq!(app.resolve_bars(&explicit, "on_range", &reqs).agg(), "time:15");
        let two = App::new().strategy::<OnRange>("a").strategy::<OnRange>("b");
        assert!(two.strategy_name(&o).is_err());
    }

    #[test]
    fn custom_modes_get_options_and_strategy() {
        use std::sync::atomic::AtomicBool;
        static RAN: AtomicBool = AtomicBool::new(false);
        let app = App::new().strategy::<OnRange>("on_range").mode("probe", "test mode", |ctx| {
            ctx.expect_opts(&["horizon", "label"])?;
            assert_eq!(ctx.opt_or("horizon", 60.0)?, 120.0);
            assert_eq!(ctx.opt("label"), Some("x=y"));
            assert_eq!(ctx.strategy(), Some("on_range"));
            assert_eq!(ctx.candidates()?[0].int("n", 0), 5);
            assert_eq!(ctx.resolved_options()?.agg(), "range:30");
            RAN.store(true, Ordering::SeqCst);
            Ok(())
        });
        let run = |extra: &str| {
            let o = RunOptions::parse(&args(&format!("Cargo.toml --mode probe --param n=5 {extra}"))).unwrap();
            app.run(&o)
        };
        run("--opt horizon=120 --opt label=x=y").unwrap();
        assert!(RAN.load(Ordering::SeqCst));
        assert!(run("--opt nope=1").unwrap_err().contains("unknown --opt `nope`"));
        let o = RunOptions::parse(&args("Cargo.toml --mode missing")).unwrap();
        assert!(app.run(&o).unwrap_err().contains("registered modes: probe"));
    }

    #[test]
    fn with_indicator_is_prepared_for_every_strategy() {
        let name = accelerando::default_registry().indicator_names().first().map(|s| s.to_string());
        let Some(name) = name else { return };
        let app = App::new().with_indicator(&name);
        let reqs = app.indicator_requests(&RunOptions::default(), &StrategyRequirements::default()).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].name, name);
    }
}
