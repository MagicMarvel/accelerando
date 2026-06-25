# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## What this is

Accelerando is a high-speed, pluggable **footprint backtesting framework** in Rust (Cargo
workspace, edition 2021). It streams an order-flow feed into footprints, runs causal indicators and
strategies over them, simulates fills, reports metrics, and accelerates parameter search across all
cores (GPU-ready seam reserved). The price-shape regime detector ships as the `whitesnake` indicator.

## Commands

```powershell
cargo build                      # debug build, whole workspace
cargo build --release            # optimized (LTO, codegen-units=1, panic=abort)
cargo build -p accelerando-core  # one crate

# run a backtest from a config -> writes result.json + prints metrics
./target/release/accelerando run --config examples/run.toml

# read-only dashboard for a saved result.json
./target/release/accelerando serve --result result.json --port 8080

# interactive studio: edit params in the page, run, watch progress, save/load runs
./target/release/accelerando studio --config examples/run.toml --port 8080   # saves under ./runs/

# parallel parameter search (objective: sharpe|sortino|pnl|return|profit_factor)
./target/release/accelerando hyperopt --config examples/run.toml --algo random --evals 64 --jobs 8
```

There are **no unit tests** in the tree. Correctness is verified two ways (see Verification below):
a numeric port-check of `whitesnake` against the original detector, and driving the studio HTTP API.

## Architecture — the big picture

The whole framework hangs off one idea: **every pluggable stage is `Configurable`** — it advertises
a `ParamSpec` (declared tunable parameters + defaults) and is built from a `Params` map. The same
definitions therefore drive both a single run and the hyperopt search space, and the studio's config
form is auto-generated from them. Read these in order to understand the system:

1. `accelerando-core/src/params.rs` — `Params`, `ParamSpec`, `Configurable`. **This is the spine.**
   Understand it and the rest follows.
2. `accelerando-core/src/traits.rs` — the four extension points: `DataSource`, `FootprintAggregator`,
   `Indicator`, `Strategy`. These are **object-safe** (the engine holds `Box<dyn _>`); `Configurable`
   is deliberately a *separate* trait on the concrete types (a `Configurable: Sized` supertrait would
   break `dyn` compatibility). `DataSource::set_progress` is an optional hook with a default no-op.
3. `accelerando-core/src/event.rs` + `footprint.rs` — the data flowing through: `OrderFlowEvent`
   (Contract/Trade/AddLimit/ReduceLimit) → `Footprint` (OHLC + bid/ask `ladder`, delta, POC, plus
   indicator-written `values`/`tags`/`plots`).
4. `accelerando-core/src/engine.rs` — `run_backtest_progress`: the hot loop. Per completed footprint
   the order is **broker first** (fill last bar's intent at this open, check intrabar stop/target,
   mark equity) → **indicators enrich** → **strategy sets next-bar intent**. The hyperopt search
   calls this thousands of times.
5. `accelerando-core/src/broker.rs` + `metrics.rs` + `result.rs` — next-bar-open fill model with tick
   slippage/commission; metrics from trades + equity curve; the serializable `BacktestResult`.

Pipeline assembly and registries live above core:

- Each adapter crate (`-sources`, `-aggregators`, `-indicators`, `-strategy`) exposes three registry
  functions in its `lib.rs`: `build(name, &Params) -> Box<dyn _>`, `spec(name) -> Option<ParamSpec>`,
  `list() -> &[&str]`. **Adding an adapter = implement the trait + `Configurable`, then add one line
  to each of those three functions.** It then appears automatically in the CLI, the hyperopt search
  space, and the studio form — no other wiring.
- `accelerando-cli/src/pipeline.rs` merges each stage's `ParamSpec` into a namespaced `SearchSpace`
  (`source.*`, `aggregator.*`, `indicator.<i>.*`, `strategy.*`) and builds a `Pipeline` from a config
  plus optional sampled overrides routed back by that namespace prefix.
- `accelerando-hyperopt/src/lib.rs` is decoupled from concrete adapters: it knows only a
  `SearchSpace` and an evaluator closure (supplied by the CLI). `BatchEvaluator` is the CPU/GPU seam —
  `CpuEvaluator` uses rayon today; a `wgpu` impl can satisfy the same trait later.
- `accelerando-cli/src/studio.rs` + `accelerando-web/src/studio.html` — the interactive studio. The
  server runs the backtest on a worker thread while the request loop keeps answering `/api/progress`;
  progress is **bytes-read from the input feed** (via `ProgressHandle` + `DataSource::set_progress`,
  implemented in `bookmap_csv.rs`). Runs persist to `runs/<name>/{config,result}.json`.

## Conventions & gotchas

- **whitesnake is causal-only.** It reproduces the original detector's causal `regime` column
  (`classify_at` + the trend-pullback carry); the original's non-causal "review" relabeling is
  intentionally omitted because it peeks at future bars. Don't reintroduce it into the indicator.
- **Port-checking whitesnake:** the on-disk `price-shape-regime/target/release/*.exe` may be **stale**
  (older than its `src/main.rs`). Always `cargo build --release` the original first, then diff.
- **`panic = "abort"` in release** → `catch_unwind` cannot recover. The studio therefore *pre-validates*
  inputs (e.g. checks the data file exists before spawning a job). Keep new server entry points
  validating rather than relying on unwinding.
- **Bookmap feed:** `T` rows are `T,ts_ns,id,price,size,side,flag`; the aggressor side is column 5
  (0-indexed), value `1`/`2`. Which is the buy aggressor is feed-dependent — exposed as the
  `buy_aggressor_code` param (default 2), flip without code changes. The `c` row carries `tick_size`
  (index 6) and `multiplier` (index 7), used by the broker for $/point PnL.
- **Timestamps are nanosecond `i64`** (~1.77e18 > JS `Number.MAX_SAFE_INTEGER`). The dashboards parse
  them as `Number`; sub-ms precision loss is acceptable for display/index mapping only — never rely on
  exact ns equality in front-end code.

## Verification (how to know a change is correct)

- whitesnake parity: build the original detector, run it and `accelerando run` on the same CSV slice,
  diff the per-bar regime labels (a faithful state matches 457/457 on the ES slice; the final
  possibly-partial bar may differ).
- studio/server: drive the JSON API with curl (`/api/schema`, POST `/api/backtest`, poll
  `/api/progress`, GET `/api/result`, `/api/save`, `/api/runs`, `/api/load`). Browser automation tools
  in this environment cannot reach the host's `localhost`, but `curl` can.

## Roadmap (where to extend)

Phase 2: more aggregators (range/tick/volume — clone `aggregators/src/time.rs`), footprint imbalance /
value-area analytics, L2 order-book events, more strategies. Phase 3: `wgpu` GPU `BatchEvaluator`,
TPE/Bayesian search, walk-forward / cross-validation.
