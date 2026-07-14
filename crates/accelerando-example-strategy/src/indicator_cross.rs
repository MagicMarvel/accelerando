//! Generic indicator-cross strategy in the spirit of a Freqtrade sample strategy.
//!
//! It expects a prior indicator step to populate `ema_fast`, `ema_slow`, `rsi`, and `volume_sma`.
//! Entries are moving-average crosses with RSI and volume filters; exits are opposite crosses or
//! RSI exhaustion.

use accelerando_core::{
    Configurable, EntryIntent, Footprint, ParamSpec, Params, Plot, PortfolioSnapshot, Strategy,
    StrategyOutput, TradeSide, VisualOutput,
};

pub struct IndicatorCross {
    qty: f64,
    stop_ticks: f64,
    target_ticks: f64,
    buy_rsi_max: f64,
    sell_rsi_min: f64,
    short_rsi_min: f64,
    cover_rsi_max: f64,
    min_volume_ratio: f64,
    side: String,
    prev_fast: Option<f64>,
    prev_slow: Option<f64>,
}

impl Configurable for IndicatorCross {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .fixed_float("qty", 1.0)
            .int("stop_ticks", 40, 4, 200, 2)
            .int("target_ticks", 80, 4, 400, 2)
            .float("buy_rsi_max", 70.0, 40.0, 90.0)
            .float("sell_rsi_min", 75.0, 50.0, 95.0)
            .float("short_rsi_min", 30.0, 10.0, 60.0)
            .float("cover_rsi_max", 25.0, 5.0, 50.0)
            .float("min_volume_ratio", 0.0, 0.0, 3.0)
            .choice("side", "long_only", &["long_only", "long_short"])
    }

    fn from_params(p: &Params) -> Self {
        Self {
            qty: p.float("qty", 1.0).max(1.0),
            stop_ticks: p.int("stop_ticks", 40) as f64,
            target_ticks: p.int("target_ticks", 80) as f64,
            buy_rsi_max: p.float("buy_rsi_max", 70.0),
            sell_rsi_min: p.float("sell_rsi_min", 75.0),
            short_rsi_min: p.float("short_rsi_min", 30.0),
            cover_rsi_max: p.float("cover_rsi_max", 25.0),
            min_volume_ratio: p.float("min_volume_ratio", 0.0),
            side: p.str("side", "long_only"),
            prev_fast: None,
            prev_slow: None,
        }
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
