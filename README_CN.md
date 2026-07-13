# Accelerando 中文说明

**高速、可插拔的订单流（footprint）回测库，Rust 实现。** _The world has been accelerated._

Accelerando 把逐笔订单流数据聚合成 footprint（足迹图）K 线，依次运行因果指标和策略、用
"下根开盘成交"的 Broker 模拟撮合，最终产出可序列化的 `BacktestResult`（指标统计、逐笔成交、
资金曲线、图表数据）。它以**库**的形式嵌入到你自己的 Rust 回测程序里——策略和适配器就是普通的
Rust 代码，和框架一起编译，没有插件加载、没有配置 DSL、没有额外构建步骤。

配套的 `accelerando-web` 提供一个零构建（无 node、无打包）的浏览器工作台：K 线 / 足迹图 /
订单簿热力图、实验参数对比，以及一套完整的**手动 bar 回放（模拟盘练习）**系统。

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
| `accelerando-web` | 浏览器工作台：结果查看、实验对比、手动 bar 回放 |
| `accelerando-example-sources` | 示例数据源：`csv`（Bookmap 式行情） |
| `accelerando-example-aggregators` | 示例聚合器：`range`、`time` |
| `accelerando-example-indicators` | 示例指标：`technical`、`regime`、`stacked_imbalance`、`adaptive_imbalance`、`big_trades`、`economic_calendar` |
| `accelerando-example-strategy` | 示例策略：`indicator_cross` |
| `accelerando-hyperopt` | 纯库形式的参数搜索原语 |
| `accelerando-ml` | 因子挖掘：PerpetualBooster（GBM）训练、Spearman IC、因子有效性报告 |

刻意不提供官方 CLI crate：请写一个小的 runner 二进制，让自定义代码正常参与编译。

## 快速上手

在你的 runner 项目里以路径依赖引入：

```toml
[dependencies]
accelerando      = { path = "../accelerando/crates/accelerando" }
accelerando-core = { path = "../accelerando/crates/accelerando-core" }
accelerando-web  = { path = "../accelerando/crates/accelerando-web" }
```

组装管线并运行：

```rust
use accelerando::default_registry;
use accelerando_core::{run_backtest, BrokerConfig, ParamValue, Params, Pipeline};

fn main() {
    let registry = default_registry(); // 内置示例适配器

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
        keep_footprints: true,         // Web 图表渲染需要
        keep_liquidity_heatmap: false, // 需要 L2 订单簿热力图时打开
    };

    let result = run_backtest(pipeline);
    println!("net pnl: {:.2}", result.metrics.net_pnl);

    // 一行代码起只读浏览器查看页
    accelerando_web::serve(&result, 8080).unwrap();
}
```

大批量搜索只需要 metrics/trades/equity 时，把 `keep_footprints` 设为 `false`
（`keep_liquidity_heatmap` 保持 `false`）以省内存。

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
2. 实现 `Configurable`（`param_spec()` + `from_params()`）。
3. 注册后像内置适配器一样构建：

```rust
let mut registry = accelerando::default_registry();
registry.register_strategy::<MyStrategy>("my_strategy");
let strategy = registry.build_strategy("my_strategy", &Params::default()).unwrap();
```

参数校验是严格的：`build` 遇到未声明的参数名或非法的 choice 取值会直接 panic（附合法名单），
拼错参数不会静默落回默认值。

## 写策略时值得知道的机制

- **持仓可见**：`ctx.open_position()` 返回 `PositionInfo`（方向、均价、持有 bar 数、当前止损
  止盈），时间止损直接 `pos.bars_held >= N` 即可；策略还可实现 `on_trade_closed(&Trade)`
  在每笔平仓时收到回调。
- **绝对价 bracket**：`ctx.go_long_bracket(qty, stop_px, target_px)` / `go_short_bracket`
  用**绝对价格**下保护单——结构性止损（某个价位 + buffer）应当用它，跳空不会把止损跟着 fill
  挪走；旧的 `go_long(qty, stop_ticks, target_ticks)`（相对下根开盘的 tick 距离）仍可用。
- **成交模型**：第 `i` 根 bar 设定的意图在第 `i+1` 根开盘成交；止损止盈按该 bar 高低价盘中
  检查。同一根 bar 内先止损后止盈（保守假设）；开盘直接跳过止损价时按**开盘价**成交
  （gap-through），不会出现"在市场没到过的价格成交"。
- **线型输出用 series**：`ctx.series("vwap", v)` / `series_colored` 每 run 存一份
  `BacktestResult.series`，studio 自动连线渲染（legend 按 id 前缀分组开关），别再每根 bar
  push `Plot::Line`。
- **trade 标签**：入场前 `ctx.label_next_entry("特征文本")`，标签跟随持仓落到成交记录里，
  供之后按 setup 特征做统计。
- **时区工具**：`accelerando_core::market_time` 提供 ns→美东日期/分钟（含夏令时）、时段窗口
  解析等。
- **Sharpe 口径**：metrics 里的 sharpe 是逐 bar equity 收益的 `均值/标准差×√N`（t 统计式，
  不年化），它同时反映单笔质量和交易次数，笔数少的策略天然到不了高 Sharpe。

## 浏览器工作台（accelerando-web）

两个服务入口，按需选一个：

- `serve(&result, port)` —— 只读查看单个 `BacktestResult`，最简单。
- `Studio` builder —— 有什么能力就链上什么：

```rust
accelerando_web::Studio::new()
    .runs(summaries, |id| load_result(id))   // 实验对比 + 惰性加载结果
    .heatmap(|query| heatmap_json(query))    // 可选：窗口化订单簿热力图
    .annotations(annotation_config)          // 可选：图表标注（JSONL 持久化）
    .replay(replay_manager)                  // 可选：手动 bar 回放
    .serve(port)?;
```

内存内的完整实验可用快捷构造 `Studio::experiment(experiment).serve(port)`。首页是两个标签页：
Manual replay（手动回放）与 Quant backtests（量化回测参数树）；replay 未启用时手动页签自动隐藏。

图表页功能：K 线 / 足迹图 / 订单簿热力图三种模式；TradingView 式交互（滚轮缩放、拖轴缩放、
双击价格轴恢复自适应、十字线）；画图工具（横线 H、矩形 R、固定区间成交量分布 VP、
日内/每周/锚定 VWAP）按 run 存 localStorage 刷新不丢；ATAS 风格 stacked-imbalance 失衡区域带，
自动向右延伸到某根 K 线收盘穿透为止。

### 手动 bar 回放（ReplayManager）

构造 `ReplayManager` 传给 Studio 即可：

```rust
use std::sync::Arc;
use accelerando_web::ReplayManager;

let replay = ReplayManager::new(Arc::new(prepared), "ES 2025-06 range30", "./replay-records");
```

- **走盘**：`→` 下一根、`Shift+→` +5、Skip N、Go-to 时间；"stop on fill" 勾选后跳跃在
  成交/止损/止盈的那根 bar 自动暂停。
- **下单**：市价按钮（Buy/Sell Mkt）；空格 + 左键买/右键卖挂限价或突破单；TP/SL 从入场线标签
  上**拖出来**才生成（TradingView 风格），可移过入场价做保本/移损，线上标签显示 tick 距离和
  预期盈亏，`×` 平仓/撤单/移除保护。
- **仓位**：支持加仓（均价合并）、部分平仓、反手；持仓同时允许一张挂单。
- **回退**：每个动作（含多 bar 跳跃的每一根）都有撤销快照，`←` 逐步回退（上限 2 万步，
  重启后清空）。
- **记录**：每一步原子写入 `record_dir/<会话id>.json`（进度、持仓、逐笔成交、资金曲线、完整
  事件日志）；服务重启后同一 URL 自动恢复；记录带**数据指纹**（首根 bar 收盘时间戳），
  换数据启动时拒绝续玩、首页列表标红 "data mismatch"。
- **费用**：手续费/滑点/初始资金按会话配置，可中途修改（只影响之后的成交）。

### 回放 HTTP API

| 方法 路径 | 说明 |
| --- | --- |
| `POST /api/replay/new` | 建会话；body 可含 `start_bar`/`random_start`/`warmup_bars`/`starting_equity`/`commission_per_contract`/`slippage_ticks` |
| `GET /api/replay/sessions` | 列出全部记录（含 compatible 标记） |
| `POST /api/replay/delete?id=` | 删除记录 |
| `GET /api/replay/state?id=&since_fp=&since_eq=` | 增量取状态（只传新 bar/新 equity 点） |
| `POST /api/replay/step?id=` | 前进：`{count}` 或 `{to_ts_ms}`，`stop_on_event` 控制暂停 |
| `POST /api/replay/back?id=` | 回退 `{count}` 步 |
| `POST /api/replay/order?id=` | 下单：`{dir, order_type: market/limit/breakout, qty, entry_price?, stop?, target?}` |
| `POST /api/replay/update?id=` | 改单/改保护：`{scope: open/pending, stop?, target?, clear_stop?, clear_target?, entry_price?, order_type?}` |
| `POST /api/replay/cancel?id=` / `flatten?id=` | 撤单 / 全平 |
| `POST /api/replay/config?id=` | 改手续费/滑点 |

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

## 数据格式（Bookmap 式 CSV）

- `T` 行：`T,ts_ns,id,price,size,side,flag` —— 主动方在第 5 列（0 起数），取值 `1`/`2`。
  哪个值代表买方主动因数据源而异，通过数据源参数 `buy_aggressor_code` 配置，不用改代码。
- `c` 行的第 6 列是 `tick_size`、第 7 列是 `multiplier`；Broker 用它们做 tick 取整和
  货币盈亏换算。
- 时间戳是纳秒 `i64`（超过 JS `Number.MAX_SAFE_INTEGER`）；浏览器端仅用于显示，
  容忍亚毫秒精度损失。

## 开发

```bash
cargo build                            # 整个工作区
cargo test -p accelerando-web          # 回放引擎单元测试
cargo test -p accelerando-ml           # 因子评估单元测试（含植入信号端到端验证）
cargo run -p accelerando-web --example replay_smoke [port]   # 合成数据冒烟服务器（默认 18973）
```

前端无构建步骤：`studio.html` / `experiment.html` 直接嵌入二进制，改完重新编译即可生效。

## 许可

MIT OR Apache-2.0。
