use accelerando_core::{Configurable, Footprint, Indicator, ParamSpec, Params, Plot};
use std::collections::VecDeque;

/// Kaufman efficiency ratio over the window: |net move| / sum(|bar-to-bar moves|), 0..1.
/// Near 0 = price churns without going anywhere (range), near 1 = one-directional.
pub const EFFICIENCY_RATIO: &str = "regime_efficiency_ratio";
/// Choppiness index over the window: 100*log10(sum(TR)/(maxH-minL))/log10(n).
/// High (classically > 61.8) = range-bound, low (< 38.2) = trending.
pub const CHOPPINESS: &str = "regime_choppiness";

/// Trend-vs-range regime measures, published as continuous values so strategies can apply
/// their own (sweepable) thresholds. Values appear only once the window is full, which lets
/// consumers fail closed during warmup.
pub struct Regime {
    window: usize,
    plot_bands: bool,
    band_chop_min: f64,
    prev_close: Option<f64>,
    bars: VecDeque<(f64, f64, f64, f64)>, // (high, low, tr, |close delta|)
    closes: VecDeque<f64>,
    tr_sum: f64,
    move_sum: f64,
}

impl Configurable for Regime {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .int("window", 30, 5, 500, 1)
            .int("plot_bands", 0, 0, 1, 1)
            .float("band_chop_min", 61.8, 0.0, 100.0)
    }

    fn from_params(p: &Params) -> Self {
        Self {
            window: p.usize("window", 30).max(2),
            plot_bands: p.int("plot_bands", 0) != 0,
            band_chop_min: p.float("band_chop_min", 61.8),
            prev_close: None,
            bars: VecDeque::new(),
            closes: VecDeque::new(),
            tr_sum: 0.0,
            move_sum: 0.0,
        }
    }
}

impl Indicator for Regime {
    fn name(&self) -> &str {
        "regime"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        let tr = match self.prev_close {
            Some(prev) => (fp.high - fp.low)
                .max((fp.high - prev).abs())
                .max((fp.low - prev).abs()),
            None => fp.high - fp.low,
        };
        let dmove = self.prev_close.map(|p| (fp.close - p).abs()).unwrap_or(0.0);
        self.prev_close = Some(fp.close);

        self.bars.push_back((fp.high, fp.low, tr, dmove));
        self.closes.push_back(fp.close);
        self.tr_sum += tr;
        self.move_sum += dmove;
        // `closes` holds one extra element: the close just before the window, so the net
        // move spans exactly `window` bar-to-bar deltas.
        while self.bars.len() > self.window {
            if let Some((_, _, tr, dmove)) = self.bars.pop_front() {
                self.tr_sum -= tr;
                self.move_sum -= dmove;
            }
        }
        while self.closes.len() > self.window + 1 {
            self.closes.pop_front();
        }
        if self.bars.len() < self.window || self.closes.len() < self.window + 1 {
            return;
        }

        let net = (fp.close - self.closes[0]).abs();
        let er = if self.move_sum > 0.0 {
            (net / self.move_sum).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut hi = f64::MIN;
        let mut lo = f64::MAX;
        for &(h, l, _, _) in &self.bars {
            hi = hi.max(h);
            lo = lo.min(l);
        }
        let span = hi - lo;
        let chop = if span > 0.0 && self.tr_sum > 0.0 {
            (100.0 * (self.tr_sum / span).log10() / (self.window as f64).log10()).clamp(0.0, 100.0)
        } else {
            100.0
        };

        fp.values.insert(EFFICIENCY_RATIO.to_string(), er);
        fp.values.insert(CHOPPINESS.to_string(), chop);
        if self.plot_bands && chop >= self.band_chop_min {
            fp.plots.push(Plot::BackgroundBand {
                color: "rgba(148,163,184,0.10)".to_string(),
                label: "choppy".to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(open: f64, high: f64, low: f64, close: f64) -> Footprint {
        Footprint {
            ts_first_ns: 0,
            ts_last_ns: 1,
            open,
            high,
            low,
            close,
            volume: 1.0,
            trades: 1,
            delta: 0.0,
            poc: close,
            ladder: Vec::new(),
            values: Default::default(),
            tags: Default::default(),
            plots: Vec::new(),
        }
    }

    fn regime(window: i64) -> Regime {
        let mut p = Params::default();
        p.set("window", accelerando_core::ParamValue::Int(window));
        Regime::from_params(&p)
    }

    #[test]
    fn trending_series_scores_efficient_and_not_choppy() {
        let mut ind = regime(10);
        let mut last = None;
        for i in 0..12 {
            let px = 100.0 + i as f64;
            let mut f = fp(px, px + 0.5, px - 0.5, px + 0.4);
            ind.on_footprint(&mut f, &[]);
            last = Some(f);
        }
        let f = last.unwrap();
        assert!(f.values[EFFICIENCY_RATIO] > 0.7, "er={}", f.values[EFFICIENCY_RATIO]);
        assert!(f.values[CHOPPINESS] < 50.0, "chop={}", f.values[CHOPPINESS]);
    }

    #[test]
    fn oscillating_series_scores_inefficient_and_choppy() {
        let mut ind = regime(10);
        let mut last = None;
        for i in 0..12 {
            let px = if i % 2 == 0 { 100.0 } else { 101.0 };
            let mut f = fp(px, px + 1.0, px - 1.0, px);
            ind.on_footprint(&mut f, &[]);
            last = Some(f);
        }
        let f = last.unwrap();
        assert!(f.values[EFFICIENCY_RATIO] < 0.2, "er={}", f.values[EFFICIENCY_RATIO]);
        assert!(f.values[CHOPPINESS] > 60.0, "chop={}", f.values[CHOPPINESS]);
    }

    #[test]
    fn no_values_until_window_full() {
        let mut ind = regime(10);
        let mut f = fp(100.0, 101.0, 99.0, 100.5);
        ind.on_footprint(&mut f, &[]);
        assert!(!f.values.contains_key(EFFICIENCY_RATIO));
    }
}
