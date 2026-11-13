# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository.

## What this is

Accelerando is a high-speed, pluggable **footprint backtesting library** in Rust. It streams an
order-flow feed into footprints, runs causal indicators and strategies over them, simulates fills,
reports metrics, and exposes library primitives for parameter search, factor mining and web result
viewing.

Downstream users create their own small runner binary so custom strategies, indicators and data
sources are compiled in normally; `accelerando-app` provides a ready-made `App` for that binary.

## Commands

```powershell
cargo build                      # debug build, whole workspace
cargo build --release            # optimized (LTO, codegen-units=1, panic=abort)
cargo build -p accelerando-core  # one crate
```

Unit tests exist where behavior is self-contained: `cargo test -p accelerando-web` (replay engine),
`cargo test -p accelerando-gui` (time axis, 1s grid, smoothing, raster) and `cargo test -p accelerando-ml` (factor evaluation, incl. planted-signal end-to-end). For
engine/indicator/strategy behavior, build a downstream runner that depends on these crates and
exercises `run_backtest` with known data. `cargo run -p accelerando-web --example replay_smoke`
serves a synthetic-data studio for manual checks.

## Architecture

The framework hangs off one idea: every pluggable stage can be `Configurable`. A component advertises
a `ParamSpec` and is built from a `Params` map. Those definitions can drive app-level config UIs,
custom runners and hyperopt search spaces.

Read these in order to understand the system:

1. `accelerando-core/src/params.rs` — `Params`, `ParamSpec`, `Configurable`.
2. `accelerando-core/src/traits.rs` — the four extension points: `DataSource`,
   `FootprintAggregator`, `Indicator`, `Strategy`.
3. `accelerando-core/src/event.rs` + `footprint.rs` — `OrderFlowEvent` and enriched `Footprint`.
4. `accelerando-core/src/engine.rs` — `run_backtest_progress` hot loop. Per completed footprint:
   broker fills/marks first, indicators enrich, strategy sets next-bar intent.
5. `accelerando-core/src/broker.rs` + `metrics.rs` + `result.rs` — fill model, metrics and
   serializable `BacktestResult`.

Pipeline assembly lives in the embedding application:

- Use `accelerando::default_registry()` when you want the built-in example adapters.
- Register custom adapters with `Registry::register_source`, `register_aggregator`,
  `register_indicator`, or `register_strategy`. Each example crate exposes a `register_all`
  that the facade crate calls.
- Build a `Pipeline` in your runner and call `run_backtest`.
- Use `accelerando-hyperopt` by supplying an evaluator closure that builds and runs a pipeline.
  `SearchSpace` describes dims, `search` drives trials through a `BatchEvaluator`;
  `CpuEvaluator` runs them on rayon (a GPU impl can satisfy the same trait later).

### Strategy-authoring layer

- `accelerando-derive` — `#[derive(Configurable)]` (re-exported as `accelerando_core::Configurable`).
  Generated code uses `::accelerando_core::` paths; core has `extern crate self as accelerando_core`
  so the derive also works inside core. Also implements `ParamGroup` for `#[param(flatten)]`.
- `accelerando-core/src/ta/` — causal incremental tools (EMA/SMA/ATR, RollingStats, RollingExtrema,
  Lookback, SessionVwap, VolumeProfile, ZigZag, Cross). The ZigZag detector moved here from
  `accelerando-example-indicators` (re-exported there).
- `accelerando-core/src/session.rs` — `SessionClock`, `SessionFilter`, `TradeDiscipline`.
- `Strategy::requirements()` / `StrategyRequirements` / `IndicatorRequest` in `traits.rs`;
  `PreparedBacktestData::check_values` validates declared value keys.
- `accelerando-core/src/testkit.rs` — synthetic bars + `drive` for strategy unit tests.
- `accelerando-app` — `App` runner (CLI, cache, rayon grid, `--validate` in `validate.rs`).
  `cargo run --release -p accelerando-app --example quickstart -- --help`.
- `Trade.pnl` includes entry + exit commission (equity already did). Capped hyperopt grids are
  sampled evenly over the full cartesian product (`grid_size` reports the full size).

### Web viewing (`accelerando-web`)

- `accelerando_web::serve(&result, port)` — one-call read-only viewer for a single
  `BacktestResult`.
- `accelerando_web::Studio` — builder for a fuller workbench: `.runs(summaries, loader)` for an
  experiment index with lazily loaded run charts, `.heatmap(f)` to route `/api/heatmap?<query>`
  to the host app, `.annotations(cfg)` for labeled price/time boxes persisted to JSONL, and
  `.replay(ReplayManager)` for interactive manual bar replay at `/replay`; then `.serve(port)`.
  `Studio::experiment(exp)` is a convenience for a complete in-memory `ExperimentResult`.
- The HTML is embedded (`studio.html`, `experiment.html`) — no node, no build step.

### Native viewing (`accelerando-gui`)

- `accelerando_gui::view(result, ViewerOptions { .. })` — native egui/eframe window for one
  `BacktestResult` (blocks; call from the main thread). Price panel = 1-second liquidity heatmap,
  smoothed best-bid / best-ask lines, bars (footprint cells once bars are >= 32 px apart, as in
  the web studio), trades and overlays; optional equity panel. Everything else (equity toggle,
  metrics, trade list, display options) sits behind the toolbar's Settings button.
- Two x axes (`AxisMode`): `Bars` = one equal-width slot per footprint, pauses > 30 min become an
  empty spacer (default without heatmap); `Time` = seconds with gaps cut (default with heatmap;
  the heatmap and bid/ask lines only exist there). A pause *inside* one bar (e.g. a reverse bar
  spanning the daily break) cannot be shown — only the aggregator could split that bar.
- Chart bar type (toolbar button next to Settings): Time / Range / Reverse / Volume bars rebuilt
  from raw trades via `ViewerOptions.rebar` (`accelerando_gui::aggregate(events, &BarSpec)`), on a
  background thread. Display only — trades, equity and strategy overlays (anchored to the
  strategy's own bars) are mapped by timestamp; the backtest is not rerun. `display_bars` opens
  on a given type. `VolumeAggregator` ("volume", param `volume`) lives in
  `accelerando-example-aggregators`.
  It does not cover the experiment index, annotations or manual replay — those stay in
  `accelerando-web`.
- `data.rs` is egui-free and unit-tested: `TimeAxis` (closed-market gaps removed, x in seconds),
  `BookGrid` (snapshots forward-filled to one cell per second, BBO derived from the levels),
  `smooth` (zero-lag, i.e. centered — display only, never feed it to a strategy), `rasterize`.
- `Pipeline.keep_liquidity_heatmap = true` now records one book snapshot per second in which the
  book changed (`engine::LIQUIDITY_HEATMAP_INTERVAL_NS`), not one per footprint close.
- Depth for the viewer is a `DepthStore` (`accelerando-core/src/depth_store.rs`): one pass writes
  exact 4-hour chunks (each seeded with the full book at its start) to disk and keeps only exact
  BBO changes + a 30 s time-weighted overview in memory. The viewer draws the overview when zoomed
  out and loads the exact chunks of the visible stretch on a background thread when the view
  spans <= `EXACT_DEPTH_SPAN_NS` (12 h). `accelerando-app` (`market.rs`) caches it as
  `<cache-dir>/depth_<hash>/` (bump `STORE_VERSION` on format changes); `--no-cache` uses a
  temporary directory. ESU6 (5 weeks, 234 M depth changes): 1.7 GB peak instead of 6.3 GB.
  `view_with_market_history` still accepts a plain `DepthHistory`. Strategies never read it.
- `cargo run --release -p accelerando --bin serve_numcat` opens it on the NumCat sample
  (`--span <secs>` initial zoom, `--web` for the old browser viewer).

### Factor mining (`accelerando-ml`)

Wraps the `perpetual` gradient-boosting crate (hyperparameter-free GBM) for ranking candidate
trading factors. Push one feature vector + forward-return label per bar into a `FactorTable`
(chronological order, `f64::NAN` for missing), then `evaluate_factors(&table, train_frac, budget,
coverage_floor)` — chronological train/test split, per-factor Spearman IC and gain importance,
OOS R² and hit rate.

## Conventions & Gotchas

- `perpetual` declares `#![feature(array_ptr_get)]` even though that API is stable. The workspace's
  `.cargo/config.toml` sets the scoped escape hatch `RUSTC_BOOTSTRAP = "perpetual"` so exactly that
  crate compiles on stable rustc. Do not remove it; downstream applications depending on
  `accelerando-ml` need the same entry in their own `.cargo/config.toml`.
- `panic = "abort"` in release means `catch_unwind` cannot recover. App entry points should validate
  inputs before spawning long jobs.
- ATAS Replay Parquet v1 is the default market data format and supports ZSTD compression.
  `ParquetSource` params `start`/`end` (app: `--from`/`--to`, part of the cache keys) prune row
  groups by footer timestamp stats; without L2 interest a row filter decodes trade rows only. With
  L2 and `start`, depth before `start` is replayed to seed the book (files carry one snapshot).
  `cargo run --release -p accelerando-example-sources --example parquet_scan -- <file> trades` benchmarks reads.
  File metadata supplies `tick_size` and `multiplier` for tick rounding and currency PnL.
- Timestamps are nanosecond `i64` values. Browser display code may lose sub-millisecond precision
  when converting to JavaScript numbers; do not rely on exact nanosecond equality in frontend code.
- For charting, `BacktestResult.footprints` must be populated. Set `Pipeline.keep_footprints = true`
  in the runner that generates a result for the web viewer (set it `false` for large search jobs to
  save memory).

## Verification

- Build the workspace with `cargo build`.
- Verify downstream integration with a runner crate such as `accel-consumer`.
- For indicator/strategy changes, run a downstream runner on a known Parquet sample and compare metrics,
  trades and rendered plots.
- For viewer changes, serve a `BacktestResult` from a runner and open the localhost URL.

## Roadmap

Phase 2: more aggregators, footprint imbalance/value-area analytics, L2 order-book events, more
strategies. Phase 3: GPU `BatchEvaluator`, TPE/Bayesian search, walk-forward and cross-validation.
