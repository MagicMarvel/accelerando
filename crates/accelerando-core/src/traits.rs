//! The four pluggable stages. Each is [`Configurable`], so user crates can supply their own
//! adapters and they slot straight into the engine and the hyperopt search space.

use crate::broker::{PortfolioSnapshot, StrategyOutput};
use crate::event::{EventInterest, OrderFlowEvent};
use crate::footprint::Footprint;
use crate::progress::ProgressHandle;
use crate::result::Trade;

// The stage traits are object-safe (`dyn`) on purpose: the engine holds them behind `Box<dyn _>`.
// Concrete adapters additionally implement [`crate::params::Configurable`]; the registry builders
// use that to construct them, but the engine never needs it.

/// Produces a normalized order-flow event stream from some backing feed.
pub trait DataSource {
    /// Consume the source and yield its events in chronological order.
    fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>>;

    /// Optionally receive the event classes needed downstream, so sources can skip parsing rows
    /// that no component will consume.
    fn set_event_interest(&mut self, _interest: EventInterest) {}

    /// Optionally accept a progress handle to report input consumption (default: ignore).
    /// Sources that know their size (e.g. a CSV) should set the total and add bytes as they read.
    fn set_progress(&mut self, _progress: ProgressHandle) {}
}

/// Folds order-flow events into footprints, emitting one when a bar boundary is crossed.
pub trait FootprintAggregator {
    /// Event classes this aggregator needs. Override to skip irrelevant hot-loop calls.
    fn event_interest(&self) -> EventInterest {
        EventInterest::ALL
    }

    /// Feed one event. Returns the just-completed footprint, if this event closed a bar.
    fn on_event(&mut self, ev: &OrderFlowEvent) -> Option<Footprint>;
    /// Emit any partially-built footprint at end of stream.
    fn flush(&mut self) -> Option<Footprint>;
}

/// Enriches the stream with computed values, tags and chart overlays.
///
/// Indicators may consume the raw order-flow stream through [`Indicator::on_event`] and/or consume
/// completed footprints through [`Indicator::on_footprint`]. Footprint callbacks are causal: they
/// see only the current footprint and the completed history before it.
pub trait Indicator {
    /// Event classes this indicator consumes through [`Indicator::on_event`].
    fn event_interest(&self) -> EventInterest {
        EventInterest::NONE
    }

    /// Called once for every normalized order-flow event.
    fn on_event(&mut self, _ev: &OrderFlowEvent) {}

    /// Called once per completed footprint, in order. `history` excludes `fp`.
    fn on_footprint(&mut self, _fp: &mut Footprint, _history: &[Footprint]) {}

    /// A short identifier used for namespacing parameters and labelling outputs.
    fn name(&self) -> &str;
}

/// Decides position changes from enriched footprints.
pub trait Strategy {
    /// Event classes this strategy consumes through [`Strategy::on_event`].
    ///
    /// Built-in engines require this to remain [`EventInterest::NONE`] so strategy decisions are
    /// based on completed footprints rather than raw events.
    fn event_interest(&self) -> EventInterest {
        EventInterest::NONE
    }

    /// Called once for every normalized order-flow event.
    ///
    /// This hook is reserved for custom engines. The built-in backtest runners do not call it.
    fn on_event(
        &mut self,
        _ev: &OrderFlowEvent,
        _portfolio: &PortfolioSnapshot,
        _output: &mut StrategyOutput,
    ) {
    }

    /// Called once per completed (and indicator-enriched) footprint. Market/account state is
    /// immutable; orders and visuals are emitted into separate typed output channels.
    fn on_footprint(
        &mut self,
        _fp: &Footprint,
        _portfolio: &PortfolioSnapshot,
        _output: &mut StrategyOutput,
    ) {
    }

    /// Called once for each round-trip the broker recorded on the current footprint (fills
    /// resolve before the strategy sees the bar), right before [`Strategy::on_footprint`].
    /// Lets strategies react to their own exits without re-deriving them from position flips.
    fn on_trade_closed(&mut self, _trade: &Trade) {}
}
