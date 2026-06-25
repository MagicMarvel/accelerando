# Accelerando

**High-speed, pluggable footprint backtesting framework.** _The world has been accelerated._

Accelerando turns any order-flow feed into footprints, runs causal indicators and strategies over
them, simulates fills, and reports performance — then accelerates parameter search across all your
cores. The price-shape regime detector ships as the **Whitesnake** indicator.

## Pipeline

```
DataSource ──▶ OrderFlowEvent ──▶ FootprintAggregator ──▶ Footprint
 (pluggable)   add/reduce/trade    (time, range, tick, …)     │
                                                               ▼
 Web dashboard ◀── BacktestResult ◀── Broker ◀── Strategy ◀── Indicators (enrich)
 (Rust-served)      PnL / equity     entry/SL/TP  (pluggable)  (Whitesnake = regime)
                              ▲
                     Hyperopt searches params across every stage (parallel, GPU-ready)
```

Every stage is `Configurable`: it advertises a `ParamSpec` and is built from a `Params` map, so the
same definitions drive a single run and the hyperopt search space. Drop in your own `DataSource`,
`FootprintAggregator`, `Indicator`, or `Strategy` and it slots straight in.

## Crates

The core framework lives in `accelerando-core`. The `accelerando-example-*` crates
provide built-in example adapters for this repo; they are optional implementations
used by the default CLI/studio setup and can be replaced by downstream adapters
without changing the core framework.

| crate                             | role                                                                |
| --------------------------------- | ------------------------------------------------------------------- | -------- | ----- | ------- |
| `accelerando-core`                | event/footprint model, traits, engine, broker, metrics              |
| `accelerando-example-sources`     | built-in example data source adapters (`bookmap_csv`)               |
| `accelerando-example-aggregators` | built-in example footprint aggregators (`time`)                     |
| `accelerando-example-indicators`  | built-in example indicators — `whitesnake` (ported regime detector) |
| `accelerando-example-strategy`    | built-in example strategies — `regime_follow`                       |
| `accelerando-hyperopt`            | parallel parameter search (rayon; GPU seam reserved)                |
| `accelerando-web`                 | zero-build dashboard server (tiny_http + embedded Canvas)           |
| `accelerando-cli`                 | `accelerando` binary: `run                                          | hyperopt | serve | studio` |

These `accelerando-example-*` crates are built-in example adapters for this repo.
For downstream adopters, the real integration surface is `accelerando-core` and the
`accelerando` facade crate; you can write your own adapters and register them with
`Registry` instead of using these built-in examples.

## How to use Accelerando

Accelerando supports three main usage modes:

1. CLI mode (`run`, `hyperopt`, `serve`) using a `run.toml` config
2. Interactive Studio mode (`studio`) with auto-generated forms
3. Library mode with custom Rust code and your own registered adapters

### 1) CLI mode

Use the CLI when you want a simple backtest or hyperopt run from a file.

```powershell
cargo build --release
.\target\release\accelerando run --config examples\run.toml
.\target\release\accelerando hyperopt --config examples\run.toml --algo random --evals 64 --jobs 8
.\target\release\accelerando serve --result result.json --port 8080
```

Common flags:

- `--config <path>`: read `examples/run.toml` or your own config
- `--result <path>`: output file for `run`
- `--algo random|grid`: hyperopt algorithm
- `--evals <N>`: number of hyperopt evaluations
- `--jobs <M>`: number of worker threads for hyperopt
- `--port <port>`: HTTP port for `serve` or `studio`
- `--runs-dir <dir>`: directory for saved studio runs

### 2) Studio mode

`accelerando studio` starts a browser-based interactive UI:

```powershell
.\target\release\accelerando studio --config examples\run.toml --port 8080
```

Studio features:

- auto-generated config form driven by each adapter's `ParamSpec`
- editable parameters for source, aggregator, indicators, strategy, broker
- progress bar during long feed replay backtests
- inline result rendering: footprints, equity curve, metrics
- save/load run configs and results under `runs/`

If no `--config` is provided, Studio seeds the form from the first registered adapter of each stage.

### 3) Library mode (code integration)

If you have your own indicator or strategy crate, use Accelerando as a library and register your adapters
into the runtime `Registry`.

The `accelerando` facade exposes:

- `accelerando::default_registry()` — a registry containing the built-in example adapters
- `accelerando_core::Registry` — create an empty registry and register your own adapters
- `accelerando_core::Configurable` — implement this trait for your adapter
- `accelerando_core::Params` / `ParamValue` — build parameter maps for runtime construction

Example code:

```rust
use accelerando::{default_registry, Registry};
use accelerando_core::{Params, run_backtest, Pipeline};
use my_source::MySource;
use my_aggregator::MyAggregator;
use my_indicator::MyIndicator;
use my_strategy::MyStrategy;

fn main() {
    let mut registry = Registry::new();
    registry.register_source::<MySource>("my_source");
    registry.register_aggregator::<MyAggregator>("my_aggregator");
    registry.register_indicator::<MyIndicator>("my_indicator");
    registry.register_strategy::<MyStrategy>("my_strategy");

    let source = registry.build_source("my_source", &Params::default()).unwrap();
    let aggregator = registry.build_aggregator("my_aggregator", &Params::default()).unwrap();
    let indicator = registry.build_indicator("my_indicator", &Params::default()).unwrap();
    let strategy = registry.build_strategy("my_strategy", &Params::default()).unwrap();

    let pipeline = Pipeline {
        source,
        aggregator,
        indicators: vec![indicator],
        strategy,
        broker_cfg: Default::default(),
        keep_footprints: true,
    };

    let result = run_backtest(pipeline);
    println!("Backtest finished: {} footprints", result.footprints.len());
}
```

If you want the CLI and Studio to recognize your custom adapters, register them into the same
runtime `Registry` and use that registry for the build path.

### When `run.toml` is optional

The `run.toml` file is only required for the CLI/Studio command-line entry points.
If you launch Accelerando from Rust code and build your own `Pipeline` directly, you can skip it.

Use `run.toml` when you want:

- a reusable example config for CLI backtests
- to share a fixed data/indicator/strategy setup with others
- Studio to pre-fill the form and save/load runs

Skip `run.toml` when you want:

- fully programmatic initialization from your own Rust crate
- custom adapters registered in code only
- embedding Accelerando inside another application

## Extending with your own adapters

Each pluggable stage is backed by a trait and the `Configurable` pattern:

- `accelerando_core::DataSource`
- `accelerando_core::FootprintAggregator`
- `accelerando_core::Indicator`
- `accelerando_core::Strategy`
- `accelerando_core::Configurable`

To add your own adapter:

1. implement the appropriate stage trait for your component
2. implement `Configurable` with `param_spec()` and `from_params()`
3. register the adapter with `Registry::register_source`, `register_aggregator`,
   `register_indicator`, or `register_strategy`
4. build your pipeline from `Registry::build_*` and run it with `run_backtest`

Because the CLI and Studio use the same registry-based construction path, your custom
adapter will also work with built-in config-driven execution and hyperopt.

## Notes

- `accelerando-example-*` crates are example implementations, not mandatory for custom usage.
- `accelerando-cli` uses `accelerando::default_registry()` by default, so the included examples are
  available out of the box.
- If you want a pure custom setup, create `Registry::new()` and register only your own adapters.

## Roadmap

- **Phase 1 (done):** core engine, Bookmap CSV source, time aggregator, Whitesnake indicator,
  regime-following strategy, broker + metrics, CLI run/serve, parallel hyperopt, web dashboard.
- **Phase 2:** range/tick/volume aggregators, richer footprint analytics (imbalance, value area),
  more strategies, L2 order-book events.
- **Phase 3:** GPU batch evaluation via `wgpu` behind the `BatchEvaluator` seam; TPE/Bayesian
  search; walk-forward / cross-validation.

## License

MIT OR Apache-2.0.
