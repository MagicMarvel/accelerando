//! Incremental, causal building blocks for indicators and strategies.
//!
//! Every type here is updated one sample at a time and only ever looks at data it has already
//! seen, so it is safe to use inside `Strategy::on_footprint` / `Indicator::on_footprint` without
//! look-ahead. They are deliberately small and tested so strategies stop re-implementing (and
//! re-breaking) EMA/VWAP/value-area/ZigZag logic.
//!
//! | type | what it tracks |
//! | --- | --- |
//! | [`Ema`] / [`Sma`] | moving averages |
//! | [`RollingStats`] | rolling mean / std / z-score of the *prior* window |
//! | [`RollingExtrema`] | highest high / lowest low of the prior N samples (Donchian) |
//! | [`Atr`] | Wilder average true range |
//! | [`SessionVwap`] | session-anchored VWAP with σ bands, resets on a new day |
//! | [`VolumeProfile`] | tick-keyed volume profile, POC, value area |
//! | [`ZigZag`] | causal swing pivots (re-exported from [`zigzag`]) |
//! | [`Cross`] | detects a series crossing another series or a level |
//! | [`Lookback`] | fixed-size history buffer (`ago(n)`) |

mod average;
mod cross;
mod profile;
mod rolling;
mod vwap;
pub mod zigzag;

pub use average::{Atr, Ema, Sma};
pub use cross::{Cross, CrossDir};
pub use profile::{ValueArea, VolumeProfile};
pub use rolling::{Lookback, RollingExtrema, RollingStats};
pub use vwap::SessionVwap;
pub use zigzag::{Pivot, PivotConfirmation, PivotKind, ZigZag};
