//! A complete strategy project in one file.
//!
//! ```text
//! cargo run --release -p accelerando-app --example quickstart -- data.csv --strategy vwap_pullback
//! cargo run --release -p accelerando-app --example quickstart -- data.csv --strategy vwap_pullback --grid --max-evals 64 --validate
//! cargo run --release -p accelerando-app --example quickstart -- --list
//! ```

use accelerando_core::ta::{Cross, CrossDir, Ema, RollingStats, SessionVwap};
use accelerando_core::{
    BarCtx, Configurable, EntryIntent, Footprint, SessionClock, SessionFilter, Strategy,
    StrategyOutput, StrategyRequirements, TradeDiscipline, TradeSide,
};

/// Trend pullback: price on the trend side of session VWAP, fast EMA above slow, and the close
/// just crossed back over the fast EMA on above-average volume.
#[derive(Configurable)]
pub struct VwapPullback {
    // ---- parameters: declared once, with default and search range ----
    #[param(default = 20, min = 5, max = 60, step = 5)]
    fast: usize,
    #[param(default = 60, min = 20, max = 200, step = 20)]
    slow: usize,
    #[param(default = 0.0, min = -1.0, max = 2.0, step = 0.5)]
    min_volume_z: f64,
    #[param(default = 8, min = 4, max = 24, step = 4)]
    stop_ticks: u32,
    #[param(default = 1.5, min = 1.0, max = 3.0, step = 0.5)]
    target_r: f64,
    #[param(default = 30, min = 5, max = 120, step = 5)]
    max_hold_bars: usize,
    #[param(default = "both", choice = ["both", "long", "short"])]
    side: String,
    #[param(flatten)]
    session: SessionFilter,
    #[param(flatten)]
    discipline: TradeDiscipline,

    // ---- state: no #[param] → Default::default() ----
    clock: SessionClock,
    vwap: SessionVwap,
    ema_fast: Option<Ema>,
    ema_slow: Option<Ema>,
    volume: Option<RollingStats>,
    cross: Cross,
}

impl Strategy for VwapPullback {
    fn requirements(&self) -> StrategyRequirements {
        // Uses only OHLCV, so no indicators; declare the bar type it was designed for.
        StrategyRequirements::new().bars("time:60")
    }

    fn on_footprint(&mut self, fp: &Footprint, ctx: &BarCtx, out: &mut StrategyOutput) {
        let now = self.clock.update(fp.ts_last_ns);
        self.discipline.on_bar(now.day);

        let vwap = self.vwap.update_bar(now.day, fp);
        let fast = self.ema_fast.get_or_insert_with(|| Ema::new(self.fast)).update(fp.close);
        let slow = self.ema_slow.get_or_insert_with(|| Ema::new(self.slow)).update(fp.close);
        let vol_z = self
            .volume
            .get_or_insert_with(|| RollingStats::new(100))
            .zscore_then_push(fp.volume);
        let crossed = self.cross.update(fp.close, fast);
        out.visuals.series("vwap", vwap);
        out.visuals.series_colored("ema_fast", fast, "#60a5fa");

        if let Some(pos) = ctx.open_position.as_ref() {
            if pos.bars_held >= self.max_hold_bars {
                out.orders.exit_position();
            }
            return;
        }
        let Some(vol_z) = vol_z else { return };
        if !self.session.allows(now.minute)
            || !self.discipline.can_enter(ctx.position, ctx.pending_count)
            || vol_z < self.min_volume_z
        {
            return;
        }
        let dir = match crossed {
            Some(CrossDir::Above) if fp.close > vwap && fast > slow => TradeSide::Long,
            Some(CrossDir::Below) if fp.close < vwap && fast < slow => TradeSide::Short,
            _ => return,
        };
        if (dir == TradeSide::Long && self.side == "short")
            || (dir == TradeSide::Short && self.side == "long")
        {
            return;
        }
        let stop = self.stop_ticks as f64;
        out.orders.replace(
            EntryIntent::market(dir, 1.0)
                .tick_bracket(stop, stop * self.target_r)
                .tagged(format!("pullback volz={vol_z:.2}")),
        );
        self.discipline.record_entry();
    }
}

fn main() {
    accelerando_app::App::new()
        .strategy::<VwapPullback>("vwap_pullback")
        .run_cli();
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::testkit::{bars_from_closes, drive, eastern_ts};
    use accelerando_core::Params;

    #[test]
    fn trades_a_pullback_in_an_uptrend() {
        let mut closes: Vec<f64> = (0..150).map(|i| 100.0 + i as f64 * 0.25).collect();
        // Dip below the fast EMA, then reclaim it.
        closes.extend([134.0, 133.0, 137.5, 138.0, 139.0, 140.0, 141.0, 142.0]);
        let mut bars = bars_from_closes(eastern_ts(2026, 1, 5, 9, 40), 60, &closes);
        for (i, fp) in bars.iter_mut().enumerate() {
            fp.volume = 100.0 + (i % 7) as f64 * 10.0; // needs dispersion for the volume z-score
        }
        let params = Params::default()
            .with("min_volume_z", -1.0)
            .with("sessions", "09:30-16:00");
        let mut s = VwapPullback::build(&params);
        let run = drive(&mut s, &bars);
        assert!(!run.trades.is_empty());
        assert_eq!(run.trades[0].dir, 1);
    }
}
