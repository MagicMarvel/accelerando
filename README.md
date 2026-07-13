# Accelerando

**High-speed, pluggable footprint backtesting library for Rust.** _The world has been accelerated._

Accelerando turns a raw order-flow feed into footprint bars, runs causal indicators and strategies
over them, simulates fills with a next-bar-open broker, and returns a serializable `BacktestResult`
(metrics, trades, equity curve, chart data). It is a **library**: you embed it in your own Rust
runner, so your strategies and adapters are ordinary Rust code compiled with the framework — no
plugin loading, no config DSL, no framework build steps.

The companion `accelerando-web` crate serves a zero-build (no node, no bundler) browser workbench:
candlestick / footprint / order-book heatmap charts, experiment comparison, and a full **manual
bar-replay (sim-trading practice) system**.

[中文说明 / Chinese README](README_CN.md)

## Pipeline

```text
DataSource -> OrderFlowEvent -> FootprintAggregator -> Footprint
                                                   |
                                                   v
BacktestResult <- Broker <- Strategy <- Indicators enrich
```

Every pluggable stage implements one trait plus `Configurable`: `DataSource`,
`FootprintAggregator`, `Indicator`, `Strategy`. A `Registry` can build adapters by name and expose
each adapter's `ParamSpec` — useful for config UIs and hyperopt search spaces, optional when you
wire the pipeline directly in code.

## Crates

| crate | role |
| --- | --- |
| `accelerando-core` | event/footprint model, traits, engine, broker, metrics |
| `accelerando` | facade crate that registers the built-in example adapters |
| `accelerando-web` | browser workbench: result viewer, experiment comparison, manual bar replay |
| `accelerando-example-sources` | example data sources: `csv` (Bookmap-style feed) |
| `accelerando-example-aggregators` | example aggregators: `range`, `time` |
| `accelerando-example-indicators` | example indicators: `technical`, `regime`, `stacked_imbalance`, `adaptive_imbalance`, `big_trades`, `economic_calendar` |
| `accelerando-example-strategy` | example strategy: `indicator_cross` |
| `accelerando-orderflow-features` | `trade_flow_features` indicator (causal per-bar order-flow features: delta/volume/trade-rate z-scores, delta efficiency, CVD, VWAP/range/session location, diagonal imbalances) + the shared `FeatureExtractor`, forward labels and CSV export for ML datasets |
| `accelerando-hyperopt` | library-only parameter search primitives |
| `accelerando-ml` | factor mining: PerpetualBooster (GBM) training, Spearman IC, factor-effectiveness reports |

There is intentionally no official CLI crate: build your own small runner binary so your custom
code compiles in normally.

## Quick start

Depend on the crates by path from your runner project:

```toml
[dependencies]
accelerando      = { path = "../accelerando/crates/accelerando" }
accelerando-core = { path = "../accelerando/crates/accelerando-core" }
accelerando-web  = { path = "../accelerando/crates/accelerando-web" }
```

Build a pipeline and run it:

```rust
use accelerando::default_registry;
use accelerando_core::{run_backtest, BrokerConfig, ParamValue, Params, Pipeline};

fn main() {
    let registry = default_registry(); // built-in example adapters

    let mut source_params = Params::default();
    source_params.set("path", ParamValue::Str("C:/data/feed.csv".into()));
    source_params.set("buy_aggressor_code", ParamValue::Int(2));

    let mut agg_params = Params::default();
    agg_params.set("range_ticks", ParamValue::Int(30));

    let pipeline = Pipeline {
        source: registry.build_source("csv", &source_params).unwrap(),
        aggregator: registry.build_aggregator("range", &agg_params).unwrap(),
        indicators: vec![registry.build_indicator("technical", &Params::default()).unwrap()],
        strategy: registry.build_strategy("indicator_cross", &Params::default()).unwrap(),
        broker_cfg: BrokerConfig {
            commission_per_contract: 2.0,
            slippage_ticks: 1.0,
            starting_equity: 100_000.0,
        },
        keep_footprints: true,        // required for charting in the web viewer
        keep_liquidity_heatmap: false, // enable to render the L2 order-book heatmap
    };

    let result = run_backtest(pipeline);
    println!("net pnl: {:.2}", result.metrics.net_pnl);

    // one-call read-only browser viewer
    accelerando_web::serve(&result, 8080).unwrap();
}
```

Set `keep_footprints = false` (and leave `keep_liquidity_heatmap = false`) for large search jobs
where you only need metrics/trades/equity.

## Reusing prepared data across strategies

Parsing and aggregating a big feed is the expensive part. For multi-strategy or multi-parameter
batches, prepare the footprints once, then run each strategy over the shared data:

```rust
use accelerando_core::{prepare_backtest_data, run_prepared_backtest, BrokerConfig};

let prepared = prepare_backtest_data(source, aggregator, indicators, /*keep_liquidity_heatmap*/ false, None);
// `prepared` is Clone + Serialize: cache it, share it across threads.

for strategy in strategies {
    let result = run_prepared_backtest(&prepared, strategy, BrokerConfig::default(), true);
    println!("{:.2}", result.metrics.net_pnl);
}
```

`PreparedBacktestData` is also what the manual-replay system steps through (see below).

## Custom adapters

1. Implement the stage trait (`DataSource`, `FootprintAggregator`, `Indicator` or `Strategy`).
2. Implement `Configurable` (`param_spec()` + `from_params()`).
3. Register it and build it like any built-in:

```rust
let mut registry = accelerando::default_registry();
registry.register_strategy::<MyStrategy>("my_strategy");
let strategy = registry.build_strategy("my_strategy", &Params::default()).unwrap();
```

Parameter validation is strict: `build` panics on unknown param names or illegal choice values
(with the list of valid names), so a typo cannot silently fall back to defaults.

## Mechanics worth knowing when writing strategies

- **Position visibility**: `ctx.open_position()` returns `PositionInfo` (direction, average price,
  bars held, current stop/target) — a time stop is just `pos.bars_held >= N`. Strategies can also
  implement `on_trade_closed(&Trade)` to be called back on every round-trip close.
- **Absolute-price brackets**: `ctx.go_long_bracket(qty, stop_px, target_px)` /
  `go_short_bracket` place protection at **absolute prices** — use these for structural stops
  (a level plus buffer) so an open gap cannot silently move the stop with the fill. The relative
  form `go_long(qty, stop_ticks, target_ticks)` (ticks from next bar's open) still exists.
- **Fill model**: intent set on bar `i` fills at bar `i+1`'s open; stops/targets are checked
  intrabar against high/low. Within one bar the stop is assumed to hit before the target
  (conservative). If the open gaps through the stop, the fill is at the **open price** — never at
  a price the market did not trade.
- **Line outputs use series**: `ctx.series("vwap", v)` / `series_colored` store one series per run
  in `BacktestResult.series`; the studio renders them as polylines with a legend grouped by id
  prefix. Do not push a `Plot::Line` per bar.
- **Trade labels**: call `ctx.label_next_entry("setup text")` before entering; the label rides the
  position into the trade record for later per-setup statistics.
- **Time zone helpers**: `accelerando_core::market_time` converts ns timestamps to US-Eastern
  dates/minutes (DST-aware) and parses session windows.
- **Sharpe convention**: the reported sharpe is per-bar equity-return `mean/std × √N`
  (t-statistic-like, not annualized). It reflects both per-trade quality and trade count; a
  low-frequency strategy cannot reach a high value by construction.

## Web workbench (`accelerando-web`)

Two entry points:

- `serve(&result, port)` — simplest: read-only viewer for one `BacktestResult`.
- `Studio` builder — chain on whichever capabilities you need:

```rust
accelerando_web::Studio::new()
    .runs(summaries, |id| load_result(id))   // experiment index + lazily loaded run charts
    .heatmap(|query| heatmap_json(query))    // optional: windowed order-book heatmap API
    .annotations(annotation_config)          // optional: chart annotations persisted to JSONL
    .replay(replay_manager)                  // optional: manual bar replay
    .serve(port)?;
```

`Studio::experiment(experiment).serve(port)` is a shortcut for a complete in-memory
`ExperimentResult`. The landing page has two tabs — Manual replay and Quant backtests — and the
replay tab hides itself when replay is not enabled.

Chart page features: candlestick / footprint / order-book-heatmap modes with TradingView-style
interaction (wheel zoom, axis-drag scaling, double-click to re-fit, crosshair); drawing tools
(horizontal line, rectangle, fixed-range volume profile, session/weekly/anchored VWAP) persisted
per run in localStorage; ATAS-style stacked-imbalance zone bands that extend right until a close
breaks through them.

### Manual bar replay

Construct a `ReplayManager` and hand it to the Studio:

```rust
use std::sync::Arc;
use accelerando_web::ReplayManager;

let replay = ReplayManager::new(Arc::new(prepared), "ES 2025-06 range30", "./replay-records");
```

- **Stepping**: `→` next bar, `Shift+→` +5, skip N, go-to time; "stop on fill" pauses a jump on
  the bar where a fill/stop/target lands.
- **Orders**: market buttons; space + left/right click places limit or breakout orders; TP/SL are
  dragged out from the entry-line label (TradingView-style), can cross entry for break-even /
  trailing, labels show tick distance and expected PnL.
- **Positions**: scale-in (average-price merge), partial close, reversal; one working order
  allowed alongside a position.
- **Undo**: every action (including each bar of a multi-bar jump) snapshots; `←` steps back
  (20k-step cap, cleared on restart).
- **Recording**: every step is written atomically to `record_dir/<session>.json` (progress,
  position, trades, equity, full event log); sessions auto-resume after a server restart. Records
  carry a data fingerprint (first bar's close timestamp) — restarting with different data refuses
  to resume and flags "data mismatch" in the index.
- **Costs**: commission/slippage/starting equity are per-session and editable mid-session
  (affects subsequent fills only).

Replay HTTP API (all under the same server):

| method & path | purpose |
| --- | --- |
| `POST /api/replay/new` | create session; body may set `start_bar`/`random_start`/`warmup_bars`/`starting_equity`/`commission_per_contract`/`slippage_ticks` |
| `GET /api/replay/sessions` | list records (with compatibility flag) |
| `POST /api/replay/delete?id=` | delete a record |
| `GET /api/replay/state?id=&since_fp=&since_eq=` | incremental state (only new bars / equity points) |
| `POST /api/replay/step?id=` | advance: `{count}` or `{to_ts_ms}`; `stop_on_event` controls pausing |
| `POST /api/replay/back?id=` | undo `{count}` steps |
| `POST /api/replay/order?id=` | place order: `{dir, order_type: market/limit/breakout, qty, entry_price?, stop?, target?}` |
| `POST /api/replay/update?id=` | modify order/protection: `{scope: open/pending, stop?, target?, clear_stop?, clear_target?, entry_price?, order_type?}` |
| `POST /api/replay/cancel?id=` / `flatten?id=` | cancel order / close everything |
| `POST /api/replay/config?id=` | change commission/slippage |

## Hyperopt

`accelerando-hyperopt` is library-only: you describe a `SearchSpace` and supply an evaluator
closure that builds a pipeline from sampled params and returns a score. `CpuEvaluator` fans trials
out on rayon; the `BatchEvaluator` trait is the seam for a future GPU implementation.

```rust
use accelerando_hyperopt::{search, Algo, CpuEvaluator};

let evaluator = CpuEvaluator {
    func: |params: &Params| {
        let pipeline = build_pipeline(params); // your function
        run_backtest(pipeline).metrics.sharpe
    },
};
let report = search(&space, Algo::Random, /*evals*/ 64, /*seed*/ 42, &evaluator);
println!("best score {:.3}", report.best.score);
```

Because the evaluator is your closure, search works unchanged with custom strategies compiled into
your runner.

## Factor mining (`accelerando-ml`)

Wraps the [perpetual](https://github.com/perpetual-ml/perpetual) crate (PerpetualBooster,
hyperparameter-free GBM) for ranking candidate trading factors. Push one feature vector plus a
forward-return label per bar into a `FactorTable` (chronological order, `f64::NAN` for missing
values — handled natively by the booster), then evaluate:

```rust
use accelerando_ml::{evaluate_factors, FactorTable};

let mut table = FactorTable::new(&["cvd_rate", "delta_ratio", "dist_vwap"]);
for bar in &bars {
    table.push_row(&features(bar), forward_return_ticks(bar));
}
// train fraction, budget, coverage floor
let report = evaluate_factors(&table, 0.7, 1.0, 0.01)?;
for f in &report.factors {
    println!("{:<16} IC {:+.4}  importance {:.1}%", f.name, f.ic, f.importance * 100.0);
}
println!("OOS R2 {:.4}, hit {:.1}%", report.test_r2, report.test_hit_rate * 100.0);
```

`evaluate_factors` does a chronological train/test split (no look-ahead), fits a `SquaredLoss`
booster, and reports per-factor coverage, univariate Spearman IC, normalized total-gain
importance, plus out-of-sample R² and direction hit rate against a majority-sign baseline. Columns
below the coverage floor are dropped and listed instead of silently fit.

**Build note:** `perpetual` declares `#![feature(array_ptr_get)]` even though that API is stable
in current rustc. This workspace's `.cargo/config.toml` sets the scoped escape hatch
`RUSTC_BOOTSTRAP = "perpetual"` so exactly that crate compiles on the stable toolchain; downstream
applications depending on `accelerando-ml` need the same entry in their own `.cargo/config.toml`.

## Data format (Bookmap-style CSV)

- `T` rows: `T,ts_ns,id,price,size,side,flag` — aggressor side is column 5 (0-indexed), value
  `1`/`2`. Which value means buy aggressor is feed-dependent; set it via the source's
  `buy_aggressor_code` param instead of editing code.
- `c` rows carry `tick_size` (index 6) and `multiplier` (index 7); the broker uses them for tick
  rounding and currency PnL.
- Timestamps are nanosecond `i64` values (larger than JS `Number.MAX_SAFE_INTEGER`); browser code
  tolerates sub-millisecond precision loss for display only.

## Development

```bash
cargo build                            # whole workspace
cargo test -p accelerando-web          # replay engine unit tests
cargo test -p accelerando-ml           # factor evaluation tests (incl. planted-signal end-to-end)
cargo run -p accelerando-web --example replay_smoke [port]   # synthetic-data smoke server (default 18973)
```

The frontend has no build step: `studio.html` / `experiment.html` are embedded in the binary;
edit and recompile.

## License

MIT OR Apache-2.0.
