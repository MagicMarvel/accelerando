# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository.

## What this is

Accelerando is a high-speed, pluggable **footprint backtesting library** in Rust. It streams an
order-flow feed into footprints, runs causal indicators and strategies over them, simulates fills,
reports metrics, and exposes library primitives for parameter search, factor mining and web result
viewing.

There is intentionally no official CLI crate. Downstream users should create their own small runner
application so custom strategies, indicators and data sources are compiled in normally.

## Commands

```powershell
cargo build                      # debug build, whole workspace
cargo build --release            # optimized (LTO, codegen-units=1, panic=abort)
cargo build -p accelerando-core  # one crate
```

Unit tests exist where behavior is self-contained: `cargo test -p accelerando-web` (replay engine)
and `cargo test -p accelerando-ml` (factor evaluation, incl. planted-signal end-to-end). For
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

### Web viewing (`accelerando-web`)

- `accelerando_web::serve(&result, port)` — one-call read-only viewer for a single
  `BacktestResult`.
- `accelerando_web::Studio` — builder for a fuller workbench: `.runs(summaries, loader)` for an
  experiment index with lazily loaded run charts, `.heatmap(f)` to route `/api/heatmap?<query>`
  to the host app, `.annotations(cfg)` for labeled price/time boxes persisted to JSONL, and
  `.replay(ReplayManager)` for interactive manual bar replay at `/replay`; then `.serve(port)`.
  `Studio::experiment(exp)` is a convenience for a complete in-memory `ExperimentResult`.
- The HTML is embedded (`studio.html`, `experiment.html`) — no node, no build step.

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
- CSV `T` rows are `T,ts_ns,id,price,size,side,flag`; aggressor side is column 5 (0-indexed),
  value `1`/`2`. Which value means buy aggressor is feed-dependent and exposed as
  `buy_aggressor_code`.
- The CSV `c` row carries `tick_size` at index 6 and `multiplier` at index 7; the broker uses
  those for tick rounding and currency PnL.
- Timestamps are nanosecond `i64` values. Browser display code may lose sub-millisecond precision
  when converting to JavaScript numbers; do not rely on exact nanosecond equality in frontend code.
- For charting, `BacktestResult.footprints` must be populated. Set `Pipeline.keep_footprints = true`
  in the runner that generates a result for the web viewer (set it `false` for large search jobs to
  save memory).

## Verification

- Build the workspace with `cargo build`.
- Verify downstream integration with a runner crate such as `accel-consumer`.
- For indicator/strategy changes, run a downstream runner on a known CSV slice and compare metrics,
  trades and rendered plots.
- For viewer changes, serve a `BacktestResult` from a runner and open the localhost URL.

## Roadmap

Phase 2: more aggregators, footprint imbalance/value-area analytics, L2 order-book events, more
strategies. Phase 3: GPU `BatchEvaluator`, TPE/Bayesian search, walk-forward and cross-validation.
