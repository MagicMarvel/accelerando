# Accelerando

**High-speed, pluggable footprint backtesting framework.** _The world has been accelerated._

Accelerando turns any order-flow feed into footprints, runs causal indicators and strategies over
them, simulates fills, and reports performance — then accelerates parameter search across all your
cores (GPU-ready). The price-shape regime detector ships as the **Whitesnake** indicator.

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
| --------------------------------- | ------------------------------------------------------------------- |
| `accelerando-core`                | event/footprint model, traits, engine, broker, metrics              |
| `accelerando-example-sources`     | built-in example data source adapters (`bookmap_csv`)               |
| `accelerando-example-aggregators` | built-in example footprint aggregators (`time`)                     |
| `accelerando-example-indicators`  | built-in example indicators — `whitesnake` (ported regime detector) |
| `accelerando-example-strategy`    | built-in example strategies — `regime_follow`                       |
| `accelerando-hyperopt`            | parallel parameter search (rayon; GPU seam reserved)                |
| `accelerando-web`                 | zero-build dashboard server (tiny_http + embedded Canvas)           |
| `accelerando-cli`                 | `accelerando` binary: `run | hyperopt | serve`                      |

These `accelerando-example-*` crates are built-in example adapters for this repo.
For downstream adopters, the real integration surface is `accelerando-core` and the
`accelerando` facade crate; you can write your own adapters and register them with
`Registry` instead of using these built-in examples.

## Quick start

```powershell
# 1) build
cargo build --release

# 2) make a slice of a Bookmap replay export (optional, for speed)
Get-Content D:\bm-userdata\Feeds\ES_...replay.csv -TotalCount 2000000 |
  Set-Content D:\allProject\accelerando\es_slice.csv

# 3) run a backtest
.\target\release\accelerando run --config examples\run.toml

# 4) view a saved result (read-only dashboard)
.\target\release\accelerando serve --result result.json --port 8080
#    open http://localhost:8080

# 5) OR launch the interactive studio: edit params in the page, click 回测,
#    watch the progress bar, and save/reload runs from disk
.\target\release\accelerando studio --config examples\run.toml --port 8080
#    open http://localhost:8080  (saved runs land under .\runs\)

# 6) accelerate: search parameters across all cores
.\target\release\accelerando hyperopt --config examples\run.toml --algo random --evals 64
```

### Studio

`accelerando studio` serves an interactive page backed by a small JSON API:

- the **config form** is auto-generated from each adapter's `ParamSpec` (so new adapters get a UI
  for free); edit any parameter and click **回测** to run.
- a **progress bar** tracks bytes read from the input feed while the backtest runs on a worker
  thread, so long runs on the full feed show real progress.
- results are rendered inline (footprint chart with regime bands + trade markers, equity curve,
  metrics); click **保存** to persist the config + result under `runs/<name>/`, and pick any
  **saved run** from the sidebar to reload its config and result later.

## Roadmap

- **Phase 1 (done):** core engine, Bookmap CSV source, time aggregator, Whitesnake indicator,
  regime-following strategy, broker + metrics, CLI run/serve, parallel hyperopt, web dashboard.
- **Phase 2:** range/tick/volume aggregators, richer footprint analytics (imbalance, value area),
  more strategies, L2 order-book events.
- **Phase 3:** GPU batch evaluation via `wgpu` behind the `BatchEvaluator` seam; TPE/Bayesian
  search; walk-forward / cross-validation.

## License

MIT OR Apache-2.0.
