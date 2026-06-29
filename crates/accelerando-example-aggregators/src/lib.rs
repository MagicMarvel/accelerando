//! Footprint aggregator adapters. Each folds the order-flow stream into footprints on a different
//! bar boundary (time, price range, tick count, traded volume). Register them with a
//! [`accelerando_core::Registry`].

mod range;
mod time;

pub use range::RangeAggregator;
pub use time::TimeAggregator;

use accelerando_core::Registry;

/// Register all built-in aggregators into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_aggregator::<RangeAggregator>("range");
    registry.register_aggregator::<TimeAggregator>("time");
}
