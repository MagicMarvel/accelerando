//! Footprint aggregator adapters. Each folds the order-flow stream into footprints on a different
//! bar boundary (time, price range, tick count, traded volume). Register them with a
//! [`accelerando_core::Registry`].

mod time;

pub use time::TimeAggregator;
