//! Indicator adapters. Each enriches footprints with values, tags and chart overlays. Register them
//! with a [`accelerando_core::Registry`].

//!
//! Typed requests provide parameter-name and enum-value completion:
//! ```
//! use accelerando_core::StrategyRequirements;
//! use accelerando_example_indicators::big_trades::{BigTradesRequest, BigTradeMode, ExecutionPrice};
//! let requirements = StrategyRequirements::new().indicator(
//!     BigTradesRequest::new().mode(BigTradeMode::Cumulative)
//!         .execution_price(ExecutionPrice::WeightedAverage).min_trade_size(300.0)
//! );
//! // Or construct an indicator directly, keeping explicitly supplied options typed.
//! let indicator = BigTradesRequest::new().mode(BigTradeMode::Single).build();
//! ```
//! Incorrect option types are rejected by the compiler:
//! ```compile_fail
//! use accelerando_example_indicators::big_trades::BigTradesRequest;
//! BigTradesRequest::new().mode("cumulative");
//! ```
//! ```compile_fail
//! use accelerando_example_indicators::big_trades::{BigTradesRequest, ThresholdMode};
//! BigTradesRequest::new().mode(ThresholdMode::Fixed);
//! ```
//! Conversion into `IndicatorRequest` retains the existing serializable `Params` format
//! for registry construction, CLI/TOML, hyperopt and cache compatibility. Indicator state
//! uses enums; `Request::build()` assigns explicit enum options without a text round trip.

pub mod adaptive_imbalance;
pub mod big_trades;
pub mod economic_calendar;
pub mod regime;
pub mod stacked_imbalance;
mod technical;
pub mod zigzag;

pub use adaptive_imbalance::{AdaptiveImbalance, AdaptiveImbalanceRequest};
pub use big_trades::{BigTrades, BigTradesRequest};
pub use economic_calendar::{EconomicCalendar, EconomicCalendarRequest};
pub use regime::{Regime, RegimeRequest};
pub use stacked_imbalance::{StackedImbalance, StackedImbalanceRequest};
pub use technical::{TechnicalIndicators, TechnicalIndicatorsRequest};
pub use zigzag::{
    Pivot, PivotConfirmation, PivotKind, ZigZag, ZigZagIndicator, ZigZagIndicatorRequest,
};

use accelerando_core::Registry;

/// Register all built-in indicators into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_indicator::<AdaptiveImbalance>("adaptive_imbalance");
    registry.register_indicator::<BigTrades>("big_trades");
    registry.register_indicator::<EconomicCalendar>("economic_calendar");
    registry.register_indicator::<Regime>("regime");
    registry.register_indicator::<StackedImbalance>("stacked_imbalance");
    registry.register_indicator::<TechnicalIndicators>("technical");
    registry.register_indicator::<ZigZagIndicator>("zigzag");
}
