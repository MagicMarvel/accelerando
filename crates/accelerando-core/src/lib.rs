//! Accelerando core — the spine of a high-speed, pluggable footprint backtester.
//!
//! Pipeline: [`DataSource`] → [`OrderFlowEvent`] → [`FootprintAggregator`] → [`Footprint`]
//! → [`Indicator`]s (enrich) → [`Strategy`] → [`Broker`] → [`BacktestResult`].
//!
//! Every pluggable stage is built from a data-driven [`Params`] map and advertises a
//! [`ParamSpec`], so the same definitions feed both a single backtest and the hyperopt search.

pub mod broker;
pub mod engine;
pub mod event;
pub mod footprint;
pub mod metrics;
pub mod params;
pub mod progress;
pub mod result;
pub mod traits;

pub use broker::{Broker, BrokerConfig, OrderCtx};
pub use engine::{run_backtest, run_backtest_progress, Pipeline};
pub use event::{OrderFlowEvent, Side};
pub use footprint::{Footprint, Level, Plot};
pub use metrics::Metrics;
pub use params::{Configurable, ParamRange, ParamSpec, ParamValue, Params};
pub use progress::{ProgressHandle, ProgressSnapshot};
pub use result::{BacktestResult, Trade, TradeReason};
pub use traits::{DataSource, FootprintAggregator, Indicator, Strategy};
