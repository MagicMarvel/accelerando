//! Accelerando core: the spine of a high-speed, pluggable footprint backtester.
//!
//! Pipeline: [`DataSource`] emits one [`OrderFlowEvent`] stream. The engine fans it out to
//! event-aware [`Indicator`]s and to the [`FootprintAggregator`], which produces display
//! footprints for footprint-aware indicators, strategies, and [`BacktestResult`].
//!
//! Every pluggable stage is built from a data-driven [`Params`] map and advertises a
//! [`ParamSpec`], so the same definitions feed both a single backtest and the hyperopt search.

pub mod broker;
pub mod engine;
pub mod event;
pub mod footprint;
#[cfg(feature = "footprint-image")]
pub mod footprint_image;
pub mod heatmap;
pub mod metrics;
pub mod params;
pub mod progress;
pub mod registry;
pub mod result;
pub mod traits;

pub use broker::{Broker, BrokerConfig, OrderCtx};
pub use engine::{
    prepare_backtest_data, run_backtest, run_backtest_progress, run_prepared_backtest, Pipeline,
    PreparedBacktestData,
};
pub use event::{EventInterest, OrderFlowEvent, Side};
pub use footprint::{Footprint, Level, Plot, VpLevel};
#[cfg(feature = "footprint-image")]
pub use footprint_image::{
    nearest_footprint, sample_trade_footprint_jpegs, TradeImageError, TradeImageManifest,
    TradeImageOptions, TradeImageRunMetrics, TradeImageSample, TradeImageWindow,
    TradeOutcomeFilter,
};
pub use heatmap::{
    parse_heatmap_query, CompactLevel, HeatmapBucket, HeatmapMetric, HeatmapQuery, HeatmapWindow,
    HiresHeatmap, SharedHeatmap, DEFAULT_HEATMAP_CACHE_DIR,
};
pub use metrics::Metrics;
pub use params::{Configurable, ParamRange, ParamSpec, ParamValue, Params};
pub use progress::{ProgressHandle, ProgressSnapshot};
pub use registry::Registry;
pub use result::{
    BacktestResult, ExperimentResult, ExperimentRun, ExperimentRunSummary, LiquidityHeatmap,
    LiquidityLevel, LiquiditySnapshot, Trade, TradeReason,
};
pub use traits::{DataSource, FootprintAggregator, Indicator, Strategy};
