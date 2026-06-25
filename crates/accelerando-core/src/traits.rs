//! The four pluggable stages. Each is [`Configurable`], so user crates can supply their own
//! adapters and they slot straight into the engine and the hyperopt search space.

use crate::broker::OrderCtx;
use crate::event::OrderFlowEvent;
use crate::footprint::Footprint;
use crate::progress::ProgressHandle;

// The stage traits are object-safe (`dyn`) on purpose: the engine holds them behind `Box<dyn _>`.
// Concrete adapters additionally implement [`crate::params::Configurable`]; the registry builders
// use that to construct them, but the engine never needs it.

/// Produces a normalized order-flow event stream from some backing feed.
pub trait DataSource {
    /// Consume the source and yield its events in chronological order.
    fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>>;

    /// Optionally accept a progress handle to report input consumption (default: ignore).
    /// Sources that know their size (e.g. a CSV) should set the total and add bytes as they read.
    fn set_progress(&mut self, _progress: ProgressHandle) {}
}

/// Folds order-flow events into footprints, emitting one when a bar boundary is crossed.
pub trait FootprintAggregator {
    /// Feed one event. Returns the just-completed footprint, if this event closed a bar.
    fn on_event(&mut self, ev: &OrderFlowEvent) -> Option<Footprint>;
    /// Emit any partially-built footprint at end of stream.
    fn flush(&mut self) -> Option<Footprint>;
}

/// Enriches a footprint with computed values, tags and chart overlays. Causal: it sees only the
/// current footprint and the completed history before it.
pub trait Indicator {
    /// Called once per completed footprint, in order. `history` excludes `fp`.
    fn on_footprint(&mut self, fp: &mut Footprint, history: &[Footprint]);
    /// A short identifier used for namespacing parameters and labelling outputs.
    fn name(&self) -> &str;
}

/// Decides position changes from enriched footprints, issuing orders through the broker context.
pub trait Strategy {
    /// Called once per completed (and indicator-enriched) footprint.
    fn on_footprint(&mut self, fp: &Footprint, ctx: &mut OrderCtx);
}
