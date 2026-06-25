//! Generic technical indicators in the style of a Freqtrade `populate_indicators` step.
//!
//! The indicator enriches each footprint with common columns a strategy can read:
//! `sma_fast`, `sma_slow`, `ema_fast`, `ema_slow`, `rsi`, `bb_lower`, `bb_mid`, `bb_upper`,
//! `bb_width`, `volume_sma`, and `delta_sma`.

use accelerando_core::{Configurable, Footprint, Indicator, ParamSpec, Params, Plot};

pub struct TechnicalIndicators {
    fast_period: usize,
    slow_period: usize,
    rsi_period: usize,
    bb_period: usize,
    bb_stddev: f64,
    volume_period: usize,
    closes: Vec<f64>,
    volumes: Vec<f64>,
    deltas: Vec<f64>,
    ema_fast: Option<f64>,
    ema_slow: Option<f64>,
}

impl Configurable for TechnicalIndicators {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .int("fast_period", 12, 2, 80, 1)
            .int("slow_period", 26, 4, 200, 1)
            .int("rsi_period", 14, 2, 100, 1)
            .int("bb_period", 20, 4, 200, 1)
            .float("bb_stddev", 2.0, 0.5, 4.0)
            .int("volume_period", 20, 2, 200, 1)
    }

    fn from_params(p: &Params) -> Self {
        let fast_period = p.usize("fast_period", 12).max(2);
        let slow_period = p.usize("slow_period", 26).max(fast_period + 1);
        Self {
            fast_period,
            slow_period,
            rsi_period: p.usize("rsi_period", 14).max(2),
            bb_period: p.usize("bb_period", 20).max(2),
            bb_stddev: p.float("bb_stddev", 2.0).max(0.1),
            volume_period: p.usize("volume_period", 20).max(2),
            closes: Vec::new(),
            volumes: Vec::new(),
            deltas: Vec::new(),
            ema_fast: None,
            ema_slow: None,
        }
    }
}

impl Indicator for TechnicalIndicators {
    fn name(&self) -> &str {
        "technical"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        self.closes.push(fp.close);
        self.volumes.push(fp.volume);
        self.deltas.push(fp.delta);

        let sma_fast = sma(&self.closes, self.fast_period);
        let sma_slow = sma(&self.closes, self.slow_period);
        let ema_fast = update_ema(&mut self.ema_fast, fp.close, self.fast_period);
        let ema_slow = update_ema(&mut self.ema_slow, fp.close, self.slow_period);
        let rsi = rsi(&self.closes, self.rsi_period);
        let (bb_lower, bb_mid, bb_upper) = bollinger(&self.closes, self.bb_period, self.bb_stddev);
        let volume_sma = sma(&self.volumes, self.volume_period);
        let delta_sma = sma(&self.deltas, self.volume_period);
        let bb_width = if bb_mid.abs() > f64::EPSILON {
            (bb_upper - bb_lower) / bb_mid.abs()
        } else {
            0.0
        };

        fp.values.insert("sma_fast".to_string(), sma_fast);
        fp.values.insert("sma_slow".to_string(), sma_slow);
        fp.values.insert("ema_fast".to_string(), ema_fast);
        fp.values.insert("ema_slow".to_string(), ema_slow);
        fp.values.insert("rsi".to_string(), rsi);
        fp.values.insert("bb_lower".to_string(), bb_lower);
        fp.values.insert("bb_mid".to_string(), bb_mid);
        fp.values.insert("bb_upper".to_string(), bb_upper);
        fp.values.insert("bb_width".to_string(), bb_width);
        fp.values.insert("volume_sma".to_string(), volume_sma);
        fp.values.insert("delta_sma".to_string(), delta_sma);

        let trend = if ema_fast > ema_slow {
            "up"
        } else if ema_fast < ema_slow {
            "down"
        } else {
            "flat"
        };
        fp.tags.insert("trend".to_string(), trend.to_string());
        fp.tags.insert(
            "rsi_state".to_string(),
            if rsi >= 70.0 {
                "overbought"
            } else if rsi <= 30.0 {
                "oversold"
            } else {
                "neutral"
            }
            .to_string(),
        );

        fp.plots.push(Plot::Line {
            id: "ema_fast".to_string(),
            value: ema_fast,
            color: "#2563eb".to_string(),
        });
        fp.plots.push(Plot::Line {
            id: "ema_slow".to_string(),
            value: ema_slow,
            color: "#f97316".to_string(),
        });
        fp.plots.push(Plot::Line {
            id: "bb_upper".to_string(),
            value: bb_upper,
            color: "#94a3b8".to_string(),
        });
        fp.plots.push(Plot::Line {
            id: "bb_lower".to_string(),
            value: bb_lower,
            color: "#94a3b8".to_string(),
        });
    }
}

fn sma(values: &[f64], period: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let n = period.min(values.len());
    values[values.len() - n..].iter().sum::<f64>() / n as f64
}

fn update_ema(state: &mut Option<f64>, value: f64, period: usize) -> f64 {
    let alpha = 2.0 / (period as f64 + 1.0);
    let next = match *state {
        Some(prev) => prev + alpha * (value - prev),
        None => value,
    };
    *state = Some(next);
    next
}

fn rsi(closes: &[f64], period: usize) -> f64 {
    if closes.len() <= period {
        return 50.0;
    }
    let start = closes.len() - period - 1;
    let mut gain = 0.0;
    let mut loss = 0.0;
    for w in closes[start..].windows(2) {
        let change = w[1] - w[0];
        if change >= 0.0 {
            gain += change;
        } else {
            loss -= change;
        }
    }
    let avg_gain = gain / period as f64;
    let avg_loss = loss / period as f64;
    if avg_loss <= f64::EPSILON {
        if avg_gain <= f64::EPSILON {
            50.0
        } else {
            100.0
        }
    } else {
        100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    }
}

fn bollinger(closes: &[f64], period: usize, stddev: f64) -> (f64, f64, f64) {
    if closes.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let n = period.min(closes.len());
    let window = &closes[closes.len() - n..];
    let mid = window.iter().sum::<f64>() / n as f64;
    let var = window.iter().map(|v| (v - mid).powi(2)).sum::<f64>() / n as f64;
    let band = stddev * var.sqrt();
    (mid - band, mid, mid + band)
}
