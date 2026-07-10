# Accelerando 中文说明

**高速、可插拔的订单流（footprint）回测框架。** _The world has been accelerated._

Accelerando 把逐笔订单流数据聚合成 footprint（足迹图）K 线，依次运行因果指标和策略、模拟成交，
最终产出可序列化的 `BacktestResult`（指标统计、逐笔成交、资金曲线、图表数据）。它以**库**的形式
嵌入到你自己的 Rust 回测程序里——策略和适配器就是普通的 Rust 代码，和框架一起编译。

配套的 `accelerando-web` 提供一个零构建（无 node、无打包）的浏览器工作台：K 线 / 足迹图 / 订单簿
热力图、实验参数树对比，以及一套完整的**手动 bar 回放（模拟盘练习）**系统。

## 数据管线

```text
DataSource -> OrderFlowEvent -> FootprintAggregator -> Footprint
                                                   |
                                                   v
BacktestResult <- Broker <- Strategy <- Indicator 逐层增强
```

每个可插拔阶段都实现对应 trait 和 `Configurable`：`DataSource`、`FootprintAggregator`、
`Indicator`、`Strategy`。`Registry` 支持按名字注册适配器并暴露 `ParamSpec` 参数说明，
供自定义 runner、Web UI 和超参搜索使用；纯代码直连时不是必需的。

## Crate 一览

| crate | 作用 |
| --- | --- |
| `accelerando-core` | 事件/footprint 模型、trait、回测引擎、Broker、指标统计 |
| `accelerando` | 门面 crate，注册内置示例适配器 |
| `accelerando-web` | 浏览器工作台：结果查看、实验对比、手动 bar 回放（详见下文） |
| `accelerando-example-sources` | 示例数据源（含 `csv`） |
| `accelerando-example-aggregators` | 示例聚合器（含 `range`、`time`） |
| `accelerando-example-indicators` | 示例指标（含 `technical`、ATAS 式对角线 `stacked_imbalance`） |
| `accelerando-example-strategy` | 示例策略 `indicator_cross` |
| `accelerando-hyperopt` | 纯库形式的参数搜索原语 |

## 作为库使用

在你的项目 `Cargo.toml` 里以路径依赖引入：

```toml
[dependencies]
accelerando-core = { path = "../accelerando/crates/accelerando-core" }
accelerando-web  = { path = "../accelerando/crates/accelerando-web" }
```

典型流程：

1. 用 `prepare_backtest_data(...)`（或自己的封装）把数据源 + 聚合器 + 指标跑成
   `PreparedBacktestData`（footprint 序列，可复用于多个策略）。
2. 对每个策略/参数组合运行回测得到 `BacktestResult`。
3. 用 `accelerando-web` 把结果挂到浏览器上。

## accelerando-web：浏览器工作台

### 服务入口

两个入口，按需选一个：

- `serve(result, port)` —— 只读查看单个 `BacktestResult`，最简单。
- `Studio` builder —— 工作台的唯一入口，有什么能力就链上什么：

```rust
accelerando_web::Studio::new()
    .runs(summaries, |id| load_result(id))   // 实验对比 + 惰性加载结果
    .heatmap(|query| heatmap_json(query))    // 可选：窗口化订单簿热力图
    .annotations(annotation_config)          // 可选：图表标注（JSONL 持久化）
    .replay(replay_manager)                  // 可选：手动 bar 回放
    .serve(port)?;
```

内存内的完整实验可用快捷构造 `Studio::experiment(experiment).serve(port)`。

首页是**两个标签页**：Manual replay（手动回测）与 Quant backtests（量化回测参数树）。
replay 未启用时手动页签自动隐藏。

### 图表页（studio）功能

- K 线 / 足迹图（footprint）/ 订单簿热力图三种模式；TradingView 式交互：滚轮缩放、拖轴缩放、
  双击价格轴恢复自适应、十字线（带 星期+日期+时间 的时间标签）
- 画图工具（左侧工具列，均为一次性工具，画完自动回到拖动模式）：
  - `―` 横线（H）、`▭` 矩形（R）、`VP` 固定区间成交量分布、`VW` VWAP（日内/每周/锚定任一 K 线）
  - 拖动模式下可直接点选图形（横线拖动、矩形四角缩放/整体平移、VP 拖边缘调范围），按 `Delete` 删除
  - 横线/矩形/VWAP 按 run 存 localStorage，刷新不丢
- 标注模式（宿主启用后）：拖框打标签，服务端 JSONL 持久化，可选中后 Delete 删除
- Stacked imbalance 区域带：`PriceBox`（group=imbalance）会画成 ATAS 风格的失衡区域，
  自动向右延伸到"某根 K 收盘穿透区域"为止（买方带被收破下沿 / 卖方带被收破上沿即终止）
- 全站右键菜单已禁用（预留给自定义菜单）

### 手动 bar 回放（ReplayManager）

宿主构造 `ReplayManager::new(prepared, source, record_dir)` 传入 serve 函数即可。核心特性：

- **走盘**：`→` 下一根、`Shift+→` +5、Skip N、Go-to 时间；"stop on fill" 勾选后跳跃在
  成交/止损/止盈的那根 bar 自动暂停
- **下单**：市价按钮（Buy/Sell Mkt）；空格 + 左键买/右键卖挂限价或突破单；下单默认只有一根
  入场线，TP/SL 从订单标签上的小按钮**拖出来**才生成（TradingView 风格），可移过入场价做保本/移损，
  线上标签显示 tick 距离和预期盈亏，`×` 平仓/撤单/移除保护
- **仓位**：支持加仓（均价合并）、部分平仓、反手；持仓同时允许一张挂单
- **回退**：每个动作（含多 bar 跳跃的每一根）都有撤销快照，`←` 逐步回退（上限 2 万步，重启后清空）
- **记录**：每一步原子写入 `record_dir/<会话id>.json`（进度、持仓、逐笔成交、资金曲线、完整事件日志）；
  服务重启后同一 URL 自动恢复；记录带**数据指纹**（首根 bar 收盘时间戳），换数据启动时拒绝续玩、
  首页列表标红 "data mismatch"
- **费用**：手续费/滑点/初始资金按会话配置，可中途修改（只影响之后的成交）

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

## 写策略时值得知道的机制

- **参数校验**：`Configurable::build` 会把传入的 Params 和 `param_spec()` 对照,**未声明的参数名
  或非法的 choice 取值直接 panic**(附合法名单),拼错参数不再静默用默认值
- **持仓可见**：`ctx.open_position()` 返回 `PositionInfo`（方向、均价、持有 bar 数、当前止损止盈）,
  时间止损直接 `pos.bars_held >= N` 即可;策略还可实现 `on_trade_closed(&Trade)` 在每笔平仓时收到回调
- **绝对价 bracket**：`ctx.go_long_bracket(qty, stop_px, target_px)` / `go_short_bracket` 用**绝对价格**
  下保护单——结构性止损（某个价位 + buffer）应当用它,跳空不会把止损跟着 fill 挪走;
  旧的 `go_long(qty, stop_ticks, target_ticks)`（相对下根开盘的 tick 距离）仍可用
- **同一根 bar 内先止损后止盈**（保守假设）;开盘直接跳过止损价时按**开盘价**成交（gap-through）,
  不会出现"在市场没到过的价格成交"
- **线型输出用 series**：`ctx.series("vwap", v)` / `series_colored` 每 run 存一份
  `BacktestResult.series`,studio 自动连线渲染（legend 按 id 前缀分组开关）,别再每根 bar push `Plot::Line`
- **trade 标签**：入场前 `ctx.label_next_entry("特征文本")`,标签跟随持仓落到成交记录 JSON 里,
  供之后按 setup 特征做统计
- **时区工具**：`accelerando_core::market_time` 提供 ns→美东日期/分钟（含夏令时）、时段窗口解析等
- **Sharpe 口径**：metrics 里的 sharpe 是逐 bar equity 收益的 `均值/标准差×√N`（t 统计式,不年化）,
  它同时反映单笔质量和交易次数,笔数少的策略天然到不了高 Sharpe

## 开发

```bash
cargo test -p accelerando-web        # 回放引擎单元测试
cargo run -p accelerando-web --example replay_smoke [port]   # 合成数据冒烟服务器（默认 18973）
```

前端无构建步骤：`studio.html` / `experiment.html` 直接嵌入二进制，改完重新编译即可生效。
