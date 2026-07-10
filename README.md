# Accelerando

**High-speed, pluggable footprint backtesting library.** _The world has been accelerated._

Accelerando turns order-flow feeds into footprints, runs causal indicators and strategies, simulates
fills, and returns a serializable `BacktestResult` with metrics, trades, equity and chart data. It is
intended to be embedded in your own Rust backtester, where your strategies and adapters are ordinary
code compiled into your application.

## Pipeline

```text
DataSource -> OrderFlowEvent -> FootprintAggregator -> Footprint
                                                   |
                                                   v
BacktestResult <- Broker <- Strategy <- Indicators enrich
```

Every pluggable stage implements a trait and `Configurable`:

- `DataSource`
- `FootprintAggregator`
- `Indicator`
- `Strategy`

The `Registry` lets an application register adapters by name and inspect each adapter's `ParamSpec`.
That is useful for custom runners, app UIs and hyperopt search spaces, but it is not required for
direct code-only construction.

## Crates

| crate | role |
| --- | --- |
| `accelerando-core` | event/footprint model, traits, engine, broker, metrics |
| `accelerando` | facade crate that registers the built-in example adapters |
| `accelerando-example-sources` | example data sources, including `csv` |
| `accelerando-example-aggregators` | example aggregators, including `range` and `time` |
| `accelerando-example-indicators` | example indicators, including `technical` |
| `accelerando-example-strategy` | example strategy, `indicator_cross` |
| `accelerando-hyperopt` | library-only parameter search primitives |
| `accelerando-ml` | factor mining: PerpetualBooster (GBM) training, Spearman IC, factor-effectiveness reports |
| `accelerando-web` | embeddable, zero-build result viewer |

There is intentionally no official CLI crate. Applications should build their own small runner so
their custom strategies, indicators and data sources are compiled in normally.

## Library Usage

Add Accelerando crates to your own Rust project, then build a pipeline in code:

```rust
use accelerando_core::{
    run_backtest, BrokerConfig, ParamValue, Params, Pipeline,
};
use accelerando::default_registry;

fn main() {
    let mut registry = default_registry();

    // Register your own adapters here.
    // registry.register_strategy::<MyStrategy>("my_strategy");

    let mut source_params = Params::default();
    source_params.set("path", ParamValue::Str("C:/data/feed.csv".to_string()));
    source_params.set("buy_aggressor_code", ParamValue::Int(2));

    let mut agg_params = Params::default();
    agg_params.set("range_ticks", ParamValue::Int(30));

    let source = registry.build_source("csv", &source_params).unwrap();
    let aggregator = registry.build_aggregator("range", &agg_params).unwrap();
    let indicator = registry.build_indicator("technical", &Params::default()).unwrap();
    let strategy = registry.build_strategy("indicator_cross", &Params::default()).unwrap();

    let pipeline = Pipeline {
        source,
        aggregator,
        indicators: vec![indicator],
        strategy,
        broker_cfg: BrokerConfig {
            commission_per_contract: 2.0,
            slippage_ticks: 1.0,
            starting_equity: 100_000.0,
        },
        keep_footprints: true,
    };

    let result = run_backtest(pipeline);
    println!("net pnl: {:.2}", result.metrics.net_pnl);

    // Optional: serve the result in the built-in browser viewer.
    accelerando_web::serve(&result, 8080).expect("serve result viewer");
}
```

Set `keep_footprints = true` when you want candlestick, footprint and plot rendering in the web
viewer. If you only need metrics/trades/equity for large search jobs, set it to `false` to reduce
memory use.

## Custom Adapters

To add a strategy, indicator, source or aggregator:

1. Implement the relevant stage trait.
2. Implement `Configurable` with `param_spec()` and `from_params()`.
3. Register it in your application's `Registry`.
4. Build the pipeline and call `run_backtest`.

```rust
let mut registry = accelerando::default_registry();
registry.register_strategy::<MyStrategy>("my_strategy");

let strategy_params = Params::default();
let strategy = registry.build_strategy("my_strategy", &strategy_params).unwrap();
```

This keeps user code explicit and Rust-native: custom strategies do not need plugin loading,
dynamic imports or framework-specific build steps.

## Result Viewer

`accelerando-web` is a library. Call it from your runner:

```rust
accelerando_web::serve(&result, 8080)?;
```

The viewer only needs a complete `BacktestResult`; it does not read the original CSV. For charting,
the result must include `footprints`, which means the pipeline that produced it must have run with
`keep_footprints: true`.

## Hyperopt

`accelerando-hyperopt` is also library-only. Your application supplies an evaluator closure that
builds a pipeline with sampled params and returns a score:

```rust
let score = |params: &Params| {
    let pipeline = build_pipeline(params);
    run_backtest(pipeline).metrics.sharpe
};
```

This keeps parameter search compatible with custom strategies compiled into your runner.

## Factor Mining (accelerando-ml)

`accelerando-ml` wraps the [perpetual](https://github.com/perpetual-ml/perpetual) gradient-boosting
crate (PerpetualBooster, hyperparameter-free GBM) for ranking candidate trading factors. Push one
feature vector plus a forward-return label per bar into a `FactorTable` (chronological order,
`f64::NAN` for missing values — handled natively by the booster), then evaluate:

```rust
use accelerando_ml::{evaluate_factors, FactorTable};

let mut table = FactorTable::new(&["cvd_rate", "delta_ratio", "dist_vwap"]);
for bar in &bars {
    table.push_row(&features(bar), forward_return_ticks(bar));
}
// train_frac, budget, coverage floor
let report = evaluate_factors(&table, 0.7, 1.0, 0.01)?;
for f in &report.factors {
    println!("{:<16} IC {:+.4}  importance {:.1}%", f.name, f.ic, f.importance * 100.0);
}
println!("OOS R2 {:.4}, hit {:.1}%", report.test_r2, report.test_hit_rate * 100.0);
```

`evaluate_factors` does a chronological train/test split (no look-ahead), fits a `SquaredLoss`
booster, and reports per-factor coverage, univariate Spearman IC, normalized total-gain importance,
plus out-of-sample R² and direction hit rate against a majority-sign baseline. Columns below the
coverage floor are dropped and listed instead of silently fit.

**Build note:** `perpetual` declares `#![feature(array_ptr_get)]` even though that API is stable in
current rustc. This workspace's `.cargo/config.toml` sets the scoped escape hatch
`RUSTC_BOOTSTRAP = "perpetual"` so exactly that crate compiles on the stable toolchain; downstream
applications depending on `accelerando-ml` need the same entry in their own `.cargo/config.toml`.

## License

MIT OR Apache-2.0.
