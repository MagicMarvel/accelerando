# Accelerando 中文说明

**高速、可插拔的订单流（footprint）回测库，Rust 实现。** _The world has been accelerated._

Accelerando 把逐笔订单流数据聚合成 footprint（足迹图）K 线，依次运行因果指标和策略、用
"下根开盘成交"的 Broker 模拟撮合，最终产出可序列化的 `BacktestResult`（指标统计、逐笔成交、
资金曲线、图表数据）。它以**库**的形式嵌入到你自己的 Rust 回测程序里——策略和适配器就是普通的
Rust 代码，和框架一起编译，没有插件加载、没有配置 DSL、没有额外构建步骤。

`accelerando-gui` 提供原生桌面图表，支持 K 线、足迹图、订单簿热力图及逐笔回放。

[English README](README.md)

## 数据管线

```text
DataSource -> OrderFlowEvent -> FootprintAggregator -> Footprint
                                                   |
                                                   v
BacktestResult <- Broker <- Strategy <- Indicator 逐层增强
```

每个可插拔阶段都实现对应 trait 和 `Configurable`：`DataSource`、`FootprintAggregator`、
`Indicator`、`Strategy`。`Registry` 支持按名字注册/构建适配器并暴露 `ParamSpec` 参数说明，
供自定义 runner、配置 UI 和超参搜索使用；纯代码直连时不是必需的。

## Crate 一览

| crate | 作用 |
| --- | --- |
| `accelerando-core` | 事件/footprint 模型、trait、回测引擎、Broker、指标统计 |
| `accelerando` | 门面 crate，注册内置示例适配器 |
| `accelerando-example-sources` | 示例数据源 `parquet`（ATAS Replay，支持 ZSTD）和 `numcat_l2`（NumCat 二级行情） |
| `accelerando-example-aggregators` | 示例聚合器：`range`、`time` |
| `accelerando-example-indicators` | 示例指标：`technical`、`regime`、`stacked_imbalance`、`adaptive_imbalance`、`big_trades`、`economic_calendar` |
| `accelerando-example-strategy` | 示例策略：`indicator_cross` |
| `accelerando-derive` | `#[derive(Configurable)]`：在结构体上一次性声明组件参数 |
| `accelerando-app` | 开箱即用的 runner：命令行、预处理缓存、并行网格、`--validate` 验证报告、桌面查看器 |
| `accelerando-hyperopt` | 纯库形式的参数搜索原语 |
| `accelerando-ml` | 因子挖掘：PerpetualBooster（GBM）训练、Spearman IC、因子有效性报告 |

仍然需要写一个自己的小 runner 二进制，让自定义代码正常参与编译；但它可以只有一行：
`accelerando_app::App::new().strategy::<MyStrategy>("my_strategy").run_cli()`（见下文“写一个策略”）。

## 快速上手

在你的 runner 项目里以路径依赖引入：

```toml
[dependencies]
accelerando      = { path = "../accelerando/crates/accelerando" }
accelerando-core = { path = "../accelerando/crates/accelerando-core" }
accelerando-gui  = { path = "../accelerando/crates/accelerando-gui" }
```

组装管线并运行：

```rust
use accelerando::default_registry;
use accelerando_core::{run_backtest, BrokerConfig, ParamValue, Params, Pipeline};

fn main() {
    let registry = default_registry(); // 内置示例适配器

    let mut source_params = Params::default();
    source_params.set("path", ParamValue::Str("C:/data/feed.parquet".into()));

    let mut agg_params = Params::default();
    agg_params.set("range_ticks", ParamValue::Int(30));

    let pipeline = Pipeline {
        source: registry.build_source("parquet", &source_params).unwrap(),
        aggregator: registry.build_aggregator("range", &agg_params).unwrap(),
        indicators: vec![registry.build_indicator("technical", &Params::default()).unwrap()],
        strategy: registry.build_strategy("indicator_cross", &Params::default()).unwrap(),
        broker_cfg: BrokerConfig {
            commission_per_contract: 2.0,
            slippage_ticks: 1.0,
            starting_equity: 100_000.0,
        },
        keep_footprints: true,         // 桌面图表渲染需要
        keep_liquidity_heatmap: false, // 需要 L2 订单簿热力图时打开
    };

    let result = run_backtest(pipeline);
    println!("net pnl: {:.2}", result.metrics.net_pnl);

    // 一行代码起只读浏览器查看页
    accelerando_gui::view(result, Default::default()).unwrap();
}
```

大批量搜索只需要 metrics/trades/equity 时，把 `keep_footprints` 设为 `false`
（`keep_liquidity_heatmap` 保持 `false`）以省内存。

## 写一个策略（推荐路径）

从想法到经过验证的回测，最短路径用到框架的四个部件。完整可运行版本见
`crates/accelerando-app/examples/quickstart.rs`。

```rust
use accelerando_core::ta::{Cross, CrossDir, Ema, SessionVwap};
use accelerando_core::{Configurable, EntryIntent, Footprint, SessionClock,
    SessionFilter, Strategy, StrategyContext, StrategyRequirements, TradeDiscipline, TradeSide};

#[derive(Configurable)]                     // 1
#[configurable(init = Self::init)]
pub struct MyStrategy {
    #[param(default = 20, min = 5, max = 60, step = 5)] fast: usize,
    #[param(default = 8, min = 4, max = 24)]            stop_ticks: u32,
    #[param(default = true)]                            exit_on_cross: bool,
    #[param(default = "both", choice = ["both", "long", "short"])] side: String,
    #[param(flatten)] session: SessionFilter,
    #[param(flatten)] discipline: TradeDiscipline,
    clock: SessionClock,                    // no #[param] -> Default::default()
    vwap: SessionVwap,
    ema: Ema,
    cross: Cross,
}
impl MyStrategy { fn init(&mut self) { self.ema = Ema::new(self.fast); } }

impl Strategy for MyStrategy {
    fn requirements(&self) -> StrategyRequirements {   // 3
        StrategyRequirements::new().bars(accelerando_core::BarSpec::Time { seconds: 60 })    // .indicator(..) / .values(&[..])
    }
    fn on_bar(&mut self, ctx: &mut StrategyContext<'_>) {
        let fp = ctx.bar();
        let portfolio = ctx.portfolio();
        let now = self.clock.update(fp.ts_last_ns);    // 2
        self.discipline.on_bar(now.day);
        let vwap = self.vwap.update_bar(now.day, fp);
        let fast = self.ema.update(fp.close);
        let crossed = self.cross.update(fp.close, fast);
        // ... ctx.replace_entry(EntryIntent::market(..).tick_bracket(..))
    }
}

fn main() {                                            // 4
    accelerando_app::App::new().strategy::<MyStrategy>("my_strategy").run_cli();
}
```

```text
cargo run --release -- data.parquet --strategy my_strategy --param stop_ticks=12 --gui
cargo run --release -- data.parquet --strategy my_strategy --grid --max-evals 200 --validate
cargo run --release -- --list
```

运行器默认读取 ATAS Replay 导出的 Parquet + ZSTD 行情文件。可用 `--source NAME` 选择其他已注册的数据源。

**Big Trades** 已内置于默认注册表。用 `--indicator big_trades:min_trade_size=50` 加到回测，也可在策略的 `requirements()` 中声明。默认采用累计成交、1 ms 时间窗、末笔成交价和固定最小成交量 25。逐笔模式用 `mode=single`。累计模式支持 `execution_price=start|last|weighted_average`，并可用 `price_location` 按 K 线位置过滤。两种模式均支持 `max_volume`，0 表示不限。

原生 GUI 默认显示 Big Trades。点击图表左上角 **Big Trades** 旁的齿轮可设置成交量上下限、标记大小、形状、内部成交量标签、买卖颜色和透明度。动态尺寸按成交量相对最低门槛的倍数线性放大，并限制在 **Min size** 和 **Max size** 之间。只有结果文件时，成交量阈值会筛选其中已保存的 Big Trades 标记。

策略绘制标记、线段或价格区域时，可设置 `group: Some("POC 回踩".into())`。图表左上角会以这个名字显示一个开关，同名图形一起显隐。可用不同名称给不同信号各设一个开关。

热图的 **Trade bubbles** 支持两个独立门槛。**Min trade size** 在聚合前筛选原始逐笔成交，默认 0。**Min displayed volume** 在聚合后筛选气泡，默认 1，Delta 模式下使用绝对 Delta。

累计模式在每根 footprint 内按首笔起算的时间窗、主动方向和扫单价格走势分组。对照本机 ATAS X 8.0.15.656，传统市场使用小于 1 ms 的时间窗，加密市场使用小于 1 秒的时间窗，可设 `cumulative_window_ms=1000`。Accelerando 的归一化成交流以单市场为前提。可选的 `threshold_mode=adaptive` 只使用已完成的 footprint 计算阈值。ATAS 的自动阈值则依赖数据提供方返回的历史累计成交。[ATAS Big Trades 文档](https://help.atas.net/en/support/solutions/articles/72000602332)

大文件可用 `--from 2026-09-02 --to 2026-09-03`（UTC，也可写 `2026-09-02T13:30` 或纳秒）只读这一段：范围外的 row group 依据 Parquet footer 统计信息直接跳过。没有组件需要盘口时只解码成交行；需要盘口时仍会扫描 `--from` 之前的盘口行来重建订单簿，并在 `--from` 时刻以 `SetLevel` 事件发出。

`--gui` 无需手动切分：盘口只读一遍，写入缓存目录下的分块存储（`depth_<hash>/`，约为 Parquet 文件的 2 倍大小）。查看器内存里只保留 30 秒粒度的概览，放大到 12 小时以内时在后台加载当前区域的精确盘口；同一文件再次 `--gui` 直接用缓存打开。

1. **`#[derive(Configurable)]`**：由字段属性生成 `param_spec()` 与 `from_params()`，且是实现
   `Configurable` 的唯一方式（trait 已封闭：参数名、默认值、范围只在字段上写一次）。浮点字段加
   `int` -> 以 `f64` 保存的整数参数；整数字段加 `time` -> 一天中的分钟，写作 `"HH:MM"`（命令行可写
   `18:00`）。整数 -> `int`
   （`min`/`max`，可选 `step`）；浮点 -> `float`（可选 `step`）；`bool` -> 开关（命令行接受
   `true/false`）；`String` 或实现 `FromStr` 的类型配 `choice = [...]`；`fixed` 表示不参与搜索；
   `name = "..."` 保留旧参数名；`#[param(flatten)]` 嵌入参数组。没有 `#[param]` 的字段以
   `Default::default()` 初始化，可选的 `init` 钩子在构造后执行。
2. **`accelerando_core::ta`**：经过测试的因果增量组件——`Ema`、`Sma`、`Atr`、`RollingStats`
   （对*之前*的窗口做 z-score）、`RollingExtrema`、`Lookback`、`SessionVwap`（含 σ 带）、
   `VolumeProfile`（按 tick 计、POC、价值区、支持滚动移除）、`ZigZag`、`Cross`。
   **`accelerando_core::session`**：`SessionClock`（ET 日期/分钟/换日）、`SessionFilter`、
   `TradeDiscipline`。
3. **`Strategy::requirements()`**：声明要运行的指标（`IndicatorRequest::new("big_trades").param("k", v)`）、
   会读取的 `Footprint::values` 键（若某个键从未发布，runner 直接报错并列出已发布的键）、
   以及策略设计时的 K 线类型。`fp.value(key)` / `fp.tag(key)` / `fp.eastern_day_minute()` 是便捷读取。
4. **`accelerando-app`**：`App` 负责 数据 -> 缓存的 footprints -> 并行回测 -> 汇总表 -> 桌面查看器 /
   JSON 导出。`--validate` 额外输出：样本内/样本外切分（样本内最优者的样本外结果、IS->OOS 秩相关）、
   配合 `--grid` 的 walk-forward 选参、滑点敏感性，以及按交易笔数/持仓时长/时段分钟匹配的随机入场基线。

单元测试：`accelerando_core::testkit` 构造合成 bar（`bar(ts).ohlc(..).value(k, v)`、
`bars_from_closes`、`eastern_ts`），并按引擎相同顺序驱动策略（`drive`、`drive_with`、`backtest`）。

记账说明：`Trade.pnl` 是完整往返净值（开仓**和**平仓手续费都已扣除），交易日志之和等于权益变化。
网格超过 `max_evals` 上限时，改为在整个网格上均匀抽样，而不是截断（截断会让后面的维度永远停在最小值）。

## 多策略复用预处理数据

解析和聚合大文件是最贵的一步。多策略 / 多参数批量回测时，先把 footprint 序列准备一次，
再让每个策略共享同一份数据：

```rust
use accelerando_core::{prepare_backtest_data, run_prepared_backtest, BrokerConfig};

let prepared = prepare_backtest_data(source, aggregator, indicators, /*keep_liquidity_heatmap*/ false, None);
// `prepared` 实现 Clone + Serialize：可以缓存到磁盘、跨线程共享。

for strategy in strategies {
    let result = run_prepared_backtest(&prepared, strategy, BrokerConfig::default(), true);
    println!("{:.2}", result.metrics.net_pnl);
}
```

手动 bar 回放系统（见下文）步进的也是同一个 `PreparedBacktestData`。

## 自定义适配器

1. 实现对应阶段的 trait（`DataSource` / `FootprintAggregator` / `Indicator` / `Strategy`）。
2. 加 `#[derive(Configurable)]`，在参数字段上写 `#[param(...)]`（手写 `impl Configurable` 无法编译；
   由参数推导的状态放进 `#[configurable(init = Self::init)]`）。
3. 注册后像内置适配器一样构建：

```rust
let mut registry = accelerando::default_registry();
registry.register_strategy::<MyStrategy>("my_strategy");
let strategy = registry.build_strategy("my_strategy", &Params::default()).unwrap();
```

参数校验是严格的：`build` 遇到未声明的参数名或非法的 choice 取值会直接 panic（附合法名单），
拼错参数不会静默落回默认值。

## 写策略时值得知道的机制

- **回调上下文**：策略实现 `on_bar(&mut self, ctx: &mut StrategyContext<'_>)`。
  通过 `ctx.bar()`、`ctx.history()`、`ctx.bar_spec()`、`ctx.portfolio()` 读取行情与账户快照，
  `ctx.bar_index()` 返回全局 K 线序号。使用 `ctx.replace_entry()`、`ctx.add_entry()`、
  `ctx.cancel_pending()`、`ctx.exit_position()` 提交交易指令，使用 `ctx.plot()`、`ctx.plot_series()` 绘图。
  框架在回调结束后处理指令，当前回调里的账户快照保持不变。逐笔策略实现
  `on_event(&mut self, ctx: &mut StrategyContext<'_>)`，通过 `ctx.event()` 读取事件。
- **持仓可见**：`ctx.portfolio().open_position` 包含 `PositionInfo`（方向、均价、持有 bar 数、当前止损
  止盈），时间止损直接 `pos.bars_held >= N` 即可；策略还可实现 `on_trade_closed(&Trade)`
  在每笔平仓时收到回调。
- **类型化入场**：用 `EntryIntent::market(...)` 或 `EntryIntent::limit(...)` 构造订单，通过
  `ctx.replace_entry(...)` / `ctx.add_entry(...)` 提交；结构止损用 `.price_bracket(...)`，相对 tick
  距离用 `.tick_bracket(...)`。
- **撤单与平仓分离**：`ctx.cancel_pending()` 只撤待成交入场单；
  `ctx.exit_position()` 才请求下一根 bar 平仓。
- **成交模型**：第 `i` 根 bar 设定的意图在第 `i+1` 根开盘成交；止损止盈按该 bar 高低价盘中
  检查。同一根 bar 内先止损后止盈（保守假设）；开盘直接跳过止损价时按**开盘价**触发
  （gap-through），不会出现"在市场没到过的价格成交"。滑点（`slippage_ticks`）对所有主动成交收取：
  市价入场/平仓，以及**被触发的止损**（止损触发后即市价单）；止盈是挂单限价，按止盈价原价成交。
  tick 止损止盈以实际（含滑点）入场成交价为锚，与 ATM 下单一致。
- **线型输出用 series**：`ctx.plot_series("vwap", v)` / `ctx.plot_series_colored()` 每 run 存一份
  `BacktestResult.series`，GUI 自动连线渲染（图例按 id 前缀分组开关），别再每根 bar
  push `Plot::Line`。
- **trade 标签原子化**：标签通过 `EntryIntent::tagged("特征文本")` 与订单一起提交，不会因
  调用顺序错误附着到之后的其他订单。
- **时区工具**：`accelerando_core::market_time` 提供 ns→美东日期/分钟（含夏令时）、时段窗口
  解析等。
- **Sharpe 口径**：metrics 里的 sharpe 是逐 bar equity 收益的 `均值/标准差×√N`（t 统计式，
  不年化），它同时反映单笔质量和交易次数，笔数少的策略天然到不了高 Sharpe。

## 在桌面查看器上绘图

通过 `--gui` 打开回测图表。在指标的 `on_footprint` 中把 `Plot` 加入 `fp.plots`，或在策略中调用 `ctx.plot(plot)`。相同 `id` 的 `Plot::Line` 点会连成一条线。GUI 也支持标记、线段、价格区间和成交量分布。

## 超参搜索（accelerando-hyperopt）

纯库形式：你描述一个 `SearchSpace`，再提供一个"用采样参数组装管线并返回得分"的闭包。
`CpuEvaluator` 用 rayon 并行跑候选；`BatchEvaluator` trait 是留给未来 GPU 实现的接口缝。

```rust
use accelerando_hyperopt::{search, Algo, CpuEvaluator};

let evaluator = CpuEvaluator {
    func: |params: &Params| {
        let pipeline = build_pipeline(params); // 你自己的组装函数
        run_backtest(pipeline).metrics.sharpe
    },
};
let report = search(&space, Algo::Random, /*evals*/ 64, /*seed*/ 42, &evaluator);
println!("best score {:.3}", report.best.score);
```

因为评估器就是你的闭包，参数搜索天然兼容编译进 runner 的自定义策略。

## 因子挖掘（accelerando-ml）

封装 [perpetual](https://github.com/perpetual-ml/perpetual)（PerpetualBooster，免调参 GBM）
用于给候选交易因子做有效性排名。按时间顺序往 `FactorTable` 里逐 bar 推入特征向量 +
未来收益标签（缺失值用 `f64::NAN`，booster 原生支持），然后评估：

```rust
use accelerando_ml::{evaluate_factors, FactorTable};

let mut table = FactorTable::new(&["cvd_rate", "delta_ratio", "dist_vwap"]);
for bar in &bars {
    table.push_row(&features(bar), forward_return_ticks(bar));
}
// 参数：训练集占比、budget、覆盖率下限
let report = evaluate_factors(&table, 0.7, 1.0, 0.01)?;
for f in &report.factors {
    println!("{:<16} IC {:+.4}  importance {:.1}%", f.name, f.ic, f.importance * 100.0);
}
println!("OOS R2 {:.4}, hit {:.1}%", report.test_r2, report.test_hit_rate * 100.0);
```

`evaluate_factors` 做**按时间**的训练/测试切分（无未来函数），拟合 `SquaredLoss` booster，
输出每因子的覆盖率、单变量 Spearman IC、归一化 TotalGain importance，以及测试集 R² 和
方向命中率（对照多数方向基线）。覆盖率低于下限的列会被剔除并在报告中列出，不会静默拟合。

**构建注意**：`perpetual` 声明了 `#![feature(array_ptr_get)]`，但该 API 在当前 rustc 里早已
stable。本工作区 `.cargo/config.toml` 里的 `RUSTC_BOOTSTRAP = "perpetual"`（作用域限定，只对
这一个 crate 生效）让它在 stable 工具链上编译；下游依赖 `accelerando-ml` 的应用需要在自己的
`.cargo/config.toml` 里加同样一条。

## 数据格式（ATAS Replay Parquet）

- ATAS Replay v1 文件按事件顺序保存成交和盘口绝对量更新。
- 文件元数据提供合约最小变动价位 `tick_size` 和乘数 `multiplier`，用于价格取整和盈亏换算。
- 时间戳是纳秒 `i64`（超过 JS `Number.MAX_SAFE_INTEGER`）；浏览器端仅用于显示，
  容忍亚毫秒精度损失。

## 开发

```bash
cargo build                            # 整个工作区
cargo test -p accelerando-gui
cargo test -p accelerando-ml           # 因子评估单元测试（含植入信号端到端验证）
```

## 许可

MIT OR Apache-2.0。
