//! Generic indicator-cross strategy in the spirit of a Freqtrade sample strategy.
//!
//! It expects a prior indicator step to populate `ema_fast`, `ema_slow`, `rsi`, and `volume_sma`.
//! Entries are moving-average crosses with RSI and volume filters; exits are opposite crosses or
//! RSI exhaustion.

use accelerando_core::{
    Configurable, EntryIntent, Footprint, Plot, PortfolioSnapshot, Strategy,
    StrategyOutput, TradeSide, VisualOutput,
};

#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct IndicatorCross {
    #[param(fixed, default = 1.0)]
    qty: f64,
    #[param(int, default = 40, min = 4, max = 200, step = 2)]
    stop_ticks: f64,
    #[param(int, default = 80, min = 4, max = 400, step = 2)]
    target_ticks: f64,
    #[param(default = 70.0, min = 40.0, max = 90.0)]
    buy_rsi_max: f64,
    #[param(default = 75.0, min = 50.0, max = 95.0)]
    sell_rsi_min: f64,
    #[param(default = 30.0, min = 10.0, max = 60.0)]
    short_rsi_min: f64,
    #[param(default = 25.0, min = 5.0, max = 50.0)]
    cover_rsi_max: f64,
    #[param(default = 0.0, min = 0.0, max = 3.0)]
    min_volume_ratio: f64,
    #[param(default = "long_only", choice = ["long_only", "long_short"])]
    side: String,
    prev_fast: Option<f64>,
    prev_slow: Option<f64>,
}

impl IndicatorCross {
    fn init(&mut self) {
        self.qty = self.qty.max(1.0);
    }
}

impl Strategy for IndicatorCross {
    fn on_footprint(
        &mut self,
        fp: &Footprint,
        portfolio: &PortfolioSnapshot,
        output: &mut StrategyOutput,
    ) {
        let Some(fast) = fp.values.get("ema_fast").copied() else {
            return;
        };
        let Some(slow) = fp.values.get("ema_slow").copied() else {
            return;
        };
        let rsi = fp.values.get("rsi").copied().unwrap_or(50.0);
        let volume_sma = fp.values.get("volume_sma").copied().unwrap_or(0.0);
        let has_volume =
            volume_sma <= f64::EPSILON || fp.volume >= volume_sma * self.min_volume_ratio.max(0.0);

        let crossed_above = self
            .prev_fast
            .zip(self.prev_slow)
            .map(|(pf, ps)| pf <= ps && fast > slow)
            .unwrap_or(false);
        let crossed_below = self
            .prev_fast
            .zip(self.prev_slow)
            .map(|(pf, ps)| pf >= ps && fast < slow)
            .unwrap_or(false);

        let pos = portfolio.position;
        if pos > 0 && (crossed_below || rsi >= self.sell_rsi_min) {
            output.orders.exit_position();
            self.plot(fp, &mut output.visuals, "exit_long", "#f97316");
        } else if pos < 0 && (crossed_above || rsi <= self.cover_rsi_max) {
            output.orders.exit_position();
            self.plot(fp, &mut output.visuals, "exit_short", "#f97316");
        } else if pos == 0 && has_volume && crossed_above && rsi <= self.buy_rsi_max {
            output.orders.replace(
                EntryIntent::market(TradeSide::Long, self.qty)
                    .tick_bracket(self.stop_ticks, self.target_ticks),
            );
            self.plot(fp, &mut output.visuals, "enter_long", "#16a34a");
        } else if pos == 0
            && self.side == "long_short"
            && has_volume
            && crossed_below
            && rsi >= self.short_rsi_min
        {
            output.orders.replace(
                EntryIntent::market(TradeSide::Short, self.qty)
                    .tick_bracket(self.stop_ticks, self.target_ticks),
            );
            self.plot(fp, &mut output.visuals, "enter_short", "#dc2626");
        }

        self.prev_fast = Some(fast);
        self.prev_slow = Some(slow);
    }
}

impl IndicatorCross {
    fn plot(&self, fp: &Footprint, visuals: &mut VisualOutput, text: &str, color: &str) {
        visuals.push(Plot::Marker {
            price: fp.close,
            shape: "triangle".to_string(),
            color: color.to_string(),
            text: text.to_string(),
            text_dx: None,
            text_dy: None,
            group: Some("signals".to_string()),
        });
    }
}
