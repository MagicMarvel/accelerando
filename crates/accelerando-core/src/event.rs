//! The normalized order-flow event that every data source adapter emits.

use serde::{Deserialize, Serialize};

/// Trade aggressor / resting-order side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

/// A single normalized change in the order flow.
///
/// Data sources translate their native format into this stream. A trades-only feed (like the
/// Bookmap ES replay) emits [`OrderFlowEvent::Contract`] once followed by [`OrderFlowEvent::Trade`]s;
/// L2-capable feeds may additionally emit [`OrderFlowEvent::AddLimit`] / [`OrderFlowEvent::ReduceLimit`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum OrderFlowEvent {
    /// Instrument metadata, emitted once before the trade stream.
    Contract { tick_size: f64, multiplier: f64 },
    /// A filled trade with its aggressor side.
    Trade {
        ts_ns: i64,
        price: f64,
        size: f64,
        aggressor: Side,
    },
    /// A new resting limit order was added to the book.
    AddLimit {
        ts_ns: i64,
        price: f64,
        size: f64,
        side: Side,
    },
    /// A resting limit order was reduced or cancelled.
    ReduceLimit {
        ts_ns: i64,
        price: f64,
        size: f64,
        side: Side,
    },
}
