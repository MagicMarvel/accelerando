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
| `accelerando-example-aggregators` | example aggregators, including `time` |
| `accelerando-example-indicators` | example indicators, including `technical` |
| `accelerando-example-strategy` | example strategy, `indicator_cross` |
| `accelerando-hyperopt` | library-only parameter search primitives |
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
    agg_params.set("bar_secs", ParamValue::Int(300));

    let source = registry.build_source("csv", &source_params).unwrap();
    let aggregator = registry.build_aggregator("time", &agg_params).unwrap();
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

## License

MIT OR Apache-2.0.
