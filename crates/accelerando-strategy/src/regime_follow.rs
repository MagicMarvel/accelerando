//! A sample strategy: follow the Whitesnake regime. Long in `trend_up`, short in `trend_down`,
//! flat otherwise, with fixed tick-based stop and target. Demonstrates the [`Strategy`] trait and
//! gives hyperopt something to tune.

use accelerando_core::{Configurable, Footprint, OrderCtx, ParamSpec, Params, Strategy};

pub struct RegimeFollow {
    qty: f64,
    stop_ticks: f64,
    target_ticks: f64,
    /// Only act on regime calls whose score clears this confidence floor.
    min_score: f64,
}

impl Configurable for RegimeFollow {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .fixed_float("qty", 1.0)
            .int("stop_ticks", 40, 4, 200, 2)
            .int("target_ticks", 80, 4, 400, 2)
            .float("min_score", 0.0, 0.0, 0.95)
    }

    fn from_params(p: &Params) -> Self {
        Self {
            qty: p.float("qty", 1.0).max(1.0),
            stop_ticks: p.int("stop_ticks", 40) as f64,
            target_ticks: p.int("target_ticks", 80) as f64,
            min_score: p.float("min_score", 0.0),
        }
    }
}

impl Strategy for RegimeFollow {
    fn on_footprint(&mut self, fp: &Footprint, ctx: &mut OrderCtx) {
        let regime = fp.tags.get("regime").map(String::as_str).unwrap_or("warmup");
        let score = fp.values.get("score").copied().unwrap_or(0.0);

        match regime {
            "trend_up" if score >= self.min_score => {
                ctx.go_long(self.qty, self.stop_ticks, self.target_ticks)
            }
            "trend_down" if score >= self.min_score => {
                ctx.go_short(self.qty, self.stop_ticks, self.target_ticks)
            }
            // In non-trending regimes, stand aside.
            "range" | "chop" | "spike" => ctx.flatten(),
            _ => { /* warmup or low score: leave position unchanged */ }
        }
    }
}
