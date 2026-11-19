use crate::broker::{EntryIntent, PortfolioSnapshot, StrategyCommands, VisualOutput};
use crate::{BarSpec, Footprint, OrderFlowEvent, Plot};

/// One strategy invocation: immutable market/account inputs and queued commands.
/// Commands are executed by the engine after the callback; the portfolio remains a snapshot.
///
/// ```
/// use accelerando_core::{EntryIntent, Strategy, StrategyContext, TradeSide};
/// struct Example;
/// impl Strategy for Example {
///     fn on_bar(&mut self, ctx: &mut StrategyContext<'_>) {
///         let bar = ctx.bar();
///         if ctx.portfolio().position_count == 0 && bar.close > bar.open {
///             ctx.replace_entry(EntryIntent::market(TradeSide::Long, 1.0));
///         }
///         ctx.plot_series("close", bar.close);
///     }
/// }
/// ```
pub struct StrategyContext<'a> {
    bar: Option<&'a Footprint>,
    event: Option<&'a OrderFlowEvent>,
    portfolio: &'a PortfolioSnapshot,
    index: usize,
    history: &'a [Footprint],
    bar_spec: Option<&'a BarSpec>,
    commands: StrategyCommands,
}

impl<'a> StrategyContext<'a> {
    pub fn for_bar(
        bar: &'a Footprint,
        portfolio: &'a PortfolioSnapshot,
        index: usize,
        visuals: bool,
    ) -> Self {
        Self {
            bar: Some(bar),
            event: None,
            portfolio,
            index,
            history: &[],
            bar_spec: None,
            commands: StrategyCommands::new(visuals),
        }
    }
    pub fn for_event(
        event: &'a OrderFlowEvent,
        portfolio: &'a PortfolioSnapshot,
        index: usize,
        visuals: bool,
    ) -> Self {
        Self {
            bar: None,
            event: Some(event),
            portfolio,
            index,
            history: &[],
            bar_spec: None,
            commands: StrategyCommands::new(visuals),
        }
    }
    /// Current completed bar, available in `Strategy::on_bar`.
    pub fn bar(&self) -> &'a Footprint {
        self.bar.expect("bar() is only available in on_bar")
    }
    /// Current raw event, available in `Strategy::on_event`.
    pub fn event(&self) -> &'a OrderFlowEvent {
        self.event.expect("event() is only available in on_event")
    }
    pub fn portfolio(&self) -> &'a PortfolioSnapshot {
        self.portfolio
    }
    /// Zero-based completed-bar index; event callbacks see the next bar's index.
    pub fn bar_index(&self) -> usize {
        self.index
    }
    /// Earlier completed, enriched bars. Never includes the current bar or future bars.
    /// The event runner resets this history at contract boundaries. Retaining history
    /// also retains completed bars during headless event runs.
    pub fn history(&self) -> &'a [Footprint] {
        self.history
    }
    /// Actual aggregation, when supplied by the aggregator or prepared-data producer.
    pub fn bar_spec(&self) -> Option<&'a BarSpec> {
        self.bar_spec
    }
    pub fn with_history(mut self, history: &'a [Footprint]) -> Self {
        self.history = history;
        self
    }
    pub fn with_bar_spec(mut self, spec: Option<&'a BarSpec>) -> Self {
        self.bar_spec = spec;
        self
    }

    pub fn replace_entry(&mut self, intent: EntryIntent) {
        self.commands.orders.replace(intent);
    }
    pub fn add_entry(&mut self, intent: EntryIntent) {
        self.commands.orders.add(intent);
    }
    pub fn cancel_pending(&mut self) {
        self.commands.orders.cancel_pending();
    }
    pub fn exit_position(&mut self) {
        self.commands.orders.exit_position();
    }
    pub fn plot(&mut self, plot: Plot) {
        self.commands.visuals.push(plot);
    }
    pub fn plot_series(&mut self, id: &str, value: f64) {
        self.commands.visuals.series(id, value);
    }
    pub fn plot_series_colored(&mut self, id: &str, value: f64, color: &str) {
        self.commands.visuals.series_colored(id, value, color);
    }
    pub fn visuals_enabled(&self) -> bool {
        self.commands.visuals.is_enabled()
    }
    /// Borrow the visual sink for reusable drawing helpers.
    pub fn visuals(&mut self) -> &mut VisualOutput {
        &mut self.commands.visuals
    }

    pub(crate) fn into_commands(self) -> StrategyCommands {
        self.commands
    }
    pub(crate) fn take_commands(&mut self) -> StrategyCommands {
        let enabled = self.visuals_enabled();
        std::mem::replace(&mut self.commands, StrategyCommands::new(enabled))
    }
    pub(crate) fn set_commands(&mut self, commands: StrategyCommands) {
        self.commands = commands;
    }
}
