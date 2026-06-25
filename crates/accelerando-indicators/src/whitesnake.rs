//! Whitesnake — the price-shape regime detector, ported from `price-shape-regime` into a
//! streaming, causal [`Indicator`].
//!
//! It keeps a ring of completed closes and, for each new footprint, reproduces the original
//! **causal** `regime` column: `classify_at` followed by the causal trend-pullback carry. The
//! non-causal "review" relabeling is intentionally omitted (it peeks at future bars).
//!
//! On each footprint it writes `tags["regime"]`, `values["score"]`, `values["regime_raw_score"]`
//! and a [`Plot::BackgroundBand`] colored by regime.

use accelerando_core::{
    Configurable, Footprint, Indicator, ParamSpec, Params, Plot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    Warmup,
    TrendUp,
    TrendDown,
    Range,
    Spike,
    Chop,
}

impl Regime {
    pub fn label(self) -> &'static str {
        match self {
            Regime::Warmup => "warmup",
            Regime::TrendUp => "trend_up",
            Regime::TrendDown => "trend_down",
            Regime::Range => "range",
            Regime::Spike => "spike",
            Regime::Chop => "chop",
        }
    }

    /// Light background-band fill (matches the original interactive chart palette).
    pub fn band_color(self) -> &'static str {
        match self {
            Regime::Warmup => "#ede9fe",
            Regime::TrendUp => "#d1fae5",
            Regime::TrendDown => "#fee2e2",
            Regime::Range => "#dbeafe",
            Regime::Spike => "#fef3c7",
            Regime::Chop => "#e5e7eb",
        }
    }
}

fn is_trend(r: Regime) -> bool {
    matches!(r, Regime::TrendUp | Regime::TrendDown)
}
fn is_neutral(r: Regime) -> bool {
    matches!(r, Regime::Range | Regime::Chop)
}

/// Tunable configuration (mirrors the original `Config`, minus aggregation/review-only fields).
#[derive(Clone, Debug)]
struct Cfg {
    window: usize,
    min_window: usize,
    smooth: usize,
    range_er: f64,
    trend_er: f64,
    trend_r2: f64,
    trend_consistency: f64,
    max_pullback: f64,
    spike_tail_share: f64,
    local_trend_window: usize,
    local_trend_er: f64,
    local_trend_r2: f64,
    local_trend_consistency: f64,
    local_trend_range_share: f64,
    local_trend_min_move: f64,
    local_trend_min_local_range: f64,
    local_range_window: usize,
    local_range_er: f64,
    local_range_crosses: usize,
    range_breakout_window: usize,
    range_breakout_min_move: f64,
    range_breakout_er: f64,
}

/// The streaming regime indicator.
pub struct Whitesnake {
    cfg: Cfg,
    closes: Vec<f64>,
    raw_regimes: Vec<Regime>,
}

impl Configurable for Whitesnake {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .int("window", 48, 12, 120, 1)
            .int("min_window", 18, 4, 60, 1)
            .int("smooth", 3, 1, 10, 1)
            .float("range_er", 0.28, 0.10, 0.45)
            .float("trend_er", 0.52, 0.35, 0.80)
            .float("trend_r2", 0.55, 0.30, 0.90)
            .float("trend_consistency", 0.68, 0.50, 0.90)
            .float("max_pullback", 0.55, 0.20, 0.90)
            .float("spike_tail_share", 0.62, 0.40, 0.90)
            .int("local_trend_window", 12, 6, 24, 1)
            .float("local_trend_er", 0.72, 0.50, 0.95)
            .float("local_trend_r2", 0.65, 0.40, 0.95)
            .float("local_trend_consistency", 0.70, 0.50, 0.95)
            .float("local_trend_range_share", 0.35, 0.15, 0.70)
            .float("local_trend_min_move", 6.0, 1.0, 20.0)
            .float("local_trend_min_local_range", 6.5, 1.0, 20.0)
            .int("local_range_window", 16, 6, 32, 1)
            .float("local_range_er", 0.30, 0.10, 0.50)
            .int("local_range_crosses", 2, 1, 6, 1)
            .int("range_breakout_window", 16, 6, 32, 1)
            .float("range_breakout_min_move", 2.0, 0.5, 10.0)
            .float("range_breakout_er", 0.35, 0.15, 0.60)
    }

    fn from_params(p: &Params) -> Self {
        let cfg = Cfg {
            window: p.usize("window", 48),
            min_window: p.usize("min_window", 18),
            smooth: p.usize("smooth", 3),
            range_er: p.float("range_er", 0.28),
            trend_er: p.float("trend_er", 0.52),
            trend_r2: p.float("trend_r2", 0.55),
            trend_consistency: p.float("trend_consistency", 0.68),
            max_pullback: p.float("max_pullback", 0.55),
            spike_tail_share: p.float("spike_tail_share", 0.62),
            local_trend_window: p.usize("local_trend_window", 12),
            local_trend_er: p.float("local_trend_er", 0.72),
            local_trend_r2: p.float("local_trend_r2", 0.65),
            local_trend_consistency: p.float("local_trend_consistency", 0.70),
            local_trend_range_share: p.float("local_trend_range_share", 0.35),
            local_trend_min_move: p.float("local_trend_min_move", 6.0),
            local_trend_min_local_range: p.float("local_trend_min_local_range", 6.5),
            local_range_window: p.usize("local_range_window", 16),
            local_range_er: p.float("local_range_er", 0.30),
            local_range_crosses: p.usize("local_range_crosses", 2),
            range_breakout_window: p.usize("range_breakout_window", 16),
            range_breakout_min_move: p.float("range_breakout_min_move", 2.0),
            range_breakout_er: p.float("range_breakout_er", 0.35),
        };
        Self {
            cfg,
            closes: Vec::new(),
            raw_regimes: Vec::new(),
        }
    }
}

impl Indicator for Whitesnake {
    fn name(&self) -> &str {
        "whitesnake"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        self.closes.push(fp.close);
        let idx = self.closes.len() - 1;
        let (raw, raw_score) = classify_at(&self.closes, idx, &self.cfg);
        self.raw_regimes.push(raw);

        // Causal trend-pullback carry: reproduce the original `carry_causal_trend_pullbacks`.
        let mut regime = raw;
        let mut score = raw_score;
        if idx >= 2 && is_neutral(raw) {
            let trend = self.raw_regimes[idx - 1];
            if is_trend(trend) && self.raw_regimes[idx - 2] == trend {
                let close = self.closes[idx];
                let prior_close = self.closes[idx - 2];
                let carries = match trend {
                    Regime::TrendUp => close >= prior_close,
                    Regime::TrendDown => close <= prior_close,
                    _ => false,
                };
                if carries {
                    regime = trend;
                    score = score.max(0.65);
                }
            }
        }

        fp.tags.insert("regime".to_string(), regime.label().to_string());
        fp.values.insert("score".to_string(), score);
        fp.values.insert("regime_raw_score".to_string(), raw_score);
        fp.plots.push(Plot::BackgroundBand {
            color: regime.band_color().to_string(),
            label: regime.label().to_string(),
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Below: pure classifier functions ported verbatim from price-shape-regime/src/main.rs.
// ---------------------------------------------------------------------------------------------

fn classify_at(closes: &[f64], idx: usize, cfg: &Cfg) -> (Regime, f64) {
    let available = idx + 1;
    if available < cfg.min_window.max(4) {
        return (Regime::Warmup, 0.0);
    }
    let len = cfg.window.min(available);
    let start = available - len;
    let raw = &closes[start..available];
    let y = smooth(raw, cfg.smooth);
    if y.len() < cfg.min_window {
        return (Regime::Warmup, 0.0);
    }

    let net = y[y.len() - 1] - y[0];
    let path: f64 = y.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    if path <= f64::EPSILON {
        return (Regime::Range, 1.0);
    }
    let er = net.abs() / path;
    let (slope, r2) = linear_fit(&y);
    let consistency = directional_consistency(&y, slope.signum());
    let pullback = max_adverse_excursion(&y, slope.signum());
    let tail_share = tail_move_share(&y, 4);
    let mid_crosses = mid_cross_count(&y);

    if tail_share >= cfg.spike_tail_share && er >= cfg.trend_er * 0.75 {
        return (Regime::Spike, tail_share);
    }
    if let Some((regime, score)) = classify_local_trend(raw, &y, cfg) {
        return (regime, score);
    }
    if let Some((regime, score)) = classify_range_breakout(raw, cfg) {
        return (regime, score);
    }
    if er <= cfg.range_er && mid_crosses >= 3 {
        return (Regime::Range, 1.0 - er);
    }
    if let Some(score) = classify_local_range(&y, cfg) {
        return (Regime::Range, score);
    }
    if er >= cfg.trend_er
        && r2 >= cfg.trend_r2
        && consistency >= cfg.trend_consistency
        && pullback <= cfg.max_pullback
        && !trend_is_stalled(raw, slope.signum())
    {
        let score = (er + r2 + consistency + (1.0 - pullback)).clamp(0.0, 4.0) / 4.0;
        if slope > 0.0 {
            return (Regime::TrendUp, score);
        }
        if slope < 0.0 {
            return (Regime::TrendDown, score);
        }
    }
    (Regime::Chop, er)
}

fn trend_is_stalled(raw: &[f64], dir: f64) -> bool {
    if dir == 0.0 || raw.len() < 4 {
        return false;
    }
    let recent = &raw[raw.len() - 4..];
    let recent_net = recent[recent.len() - 1] - recent[0];
    if dir * recent_net > 0.0 {
        return false;
    }
    if dir > 0.0 {
        let high_idx = raw
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(idx, _)| idx)
            .unwrap_or(raw.len() - 1);
        raw.len() - 1 - high_idx >= 3
    } else {
        let low_idx = raw
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .map(|(idx, _)| idx)
            .unwrap_or(raw.len() - 1);
        raw.len() - 1 - low_idx >= 3
    }
}

fn classify_local_trend(raw: &[f64], y: &[f64], cfg: &Cfg) -> Option<(Regime, f64)> {
    let max_len = cfg.local_trend_window.min(y.len());
    let min_len = 6.min(max_len);
    if max_len < 6 {
        return None;
    }
    let full_min = y.iter().fold(f64::INFINITY, |a, b| a.min(*b));
    let full_max = y.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b));
    let full_range = (full_max - full_min).max(f64::EPSILON);

    let mut best: Option<(Regime, f64)> = None;
    for len in min_len..=max_len {
        let raw_local = &raw[raw.len() - len..];
        let local = &y[y.len() - len..];
        let net = local[local.len() - 1] - local[0];
        let path: f64 = local.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        if path <= f64::EPSILON {
            continue;
        }
        let er = net.abs() / path;
        let range_share = net.abs() / full_range;
        let local_min = raw_local.iter().fold(f64::INFINITY, |a, b| a.min(*b));
        let local_max = raw_local.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b));
        let local_range = local_max - local_min;
        let (slope, r2) = linear_fit(local);
        let last_raw_step = raw_local[raw_local.len() - 1] - raw_local[raw_local.len() - 2];
        let consistency = directional_consistency(local, slope.signum());
        if er >= cfg.local_trend_er
            && r2 >= cfg.local_trend_r2
            && consistency >= cfg.local_trend_consistency
            && local_range >= cfg.local_trend_min_local_range
            && (range_share >= cfg.local_trend_range_share || net.abs() >= cfg.local_trend_min_move)
            && slope.signum() * last_raw_step >= 0.0
        {
            let regime = if slope > 0.0 {
                Regime::TrendUp
            } else if slope < 0.0 {
                Regime::TrendDown
            } else {
                continue;
            };
            let score = (er + r2 + consistency + range_share.min(1.0)).clamp(0.0, 4.0) / 4.0;
            if best.map(|(_, best_score)| score > best_score).unwrap_or(true) {
                best = Some((regime, score));
            }
        }
    }
    best
}

fn classify_range_breakout(raw: &[f64], cfg: &Cfg) -> Option<(Regime, f64)> {
    let len = cfg.range_breakout_window.min(raw.len().saturating_sub(1));
    if len < 6 {
        return None;
    }
    let current = raw[raw.len() - 1];
    let prior = &raw[raw.len() - 1 - len..raw.len() - 1];
    let net = prior[prior.len() - 1] - prior[0];
    let path: f64 = prior.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    if path <= f64::EPSILON {
        return None;
    }
    let er = net.abs() / path;
    if er > cfg.range_breakout_er && mid_cross_count(prior) < cfg.local_range_crosses {
        return None;
    }
    let prior_min = prior.iter().fold(f64::INFINITY, |a, b| a.min(*b));
    let prior_max = prior.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b));
    if current <= prior_min - cfg.range_breakout_min_move {
        let score = ((prior_min - current) / cfg.range_breakout_min_move).clamp(0.65, 1.0);
        return Some((Regime::TrendDown, score));
    }
    if current >= prior_max + cfg.range_breakout_min_move {
        let score = ((current - prior_max) / cfg.range_breakout_min_move).clamp(0.65, 1.0);
        return Some((Regime::TrendUp, score));
    }
    None
}

fn classify_local_range(y: &[f64], cfg: &Cfg) -> Option<f64> {
    let len = cfg.local_range_window.min(y.len());
    if len < 6 {
        return None;
    }
    let local = &y[y.len() - len..];
    let net = local[local.len() - 1] - local[0];
    let path: f64 = local.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    if path <= f64::EPSILON {
        return Some(1.0);
    }
    let er = net.abs() / path;
    let crosses = mid_cross_count(local);
    if er <= cfg.local_range_er && crosses >= cfg.local_range_crosses {
        return Some(1.0 - er);
    }
    None
}

fn smooth(values: &[f64], span: usize) -> Vec<f64> {
    if span <= 1 {
        return values.to_vec();
    }
    let mut out = Vec::with_capacity(values.len());
    let mut sum = 0.0;
    for (i, v) in values.iter().enumerate() {
        sum += *v;
        if i >= span {
            sum -= values[i - span];
        }
        let n = (i + 1).min(span) as f64;
        out.push(sum / n);
    }
    out
}

fn linear_fit(y: &[f64]) -> (f64, f64) {
    let n = y.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = y.iter().sum::<f64>() / n;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut sst = 0.0;
    let mut sse = 0.0;
    for (i, value) in y.iter().enumerate() {
        let x = i as f64;
        sxx += (x - mean_x) * (x - mean_x);
        sxy += (x - mean_x) * (*value - mean_y);
        sst += (*value - mean_y) * (*value - mean_y);
    }
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let intercept = mean_y - slope * mean_x;
    for (i, value) in y.iter().enumerate() {
        let fit = intercept + slope * i as f64;
        sse += (*value - fit) * (*value - fit);
    }
    let r2 = if sst > 0.0 { 1.0 - sse / sst } else { 0.0 };
    (slope, r2.clamp(0.0, 1.0))
}

fn directional_consistency(y: &[f64], dir: f64) -> f64 {
    if dir == 0.0 || y.len() < 2 {
        return 0.0;
    }
    let good = y.windows(2).filter(|w| dir * (w[1] - w[0]) > 0.0).count();
    good as f64 / (y.len() - 1) as f64
}

fn max_adverse_excursion(y: &[f64], dir: f64) -> f64 {
    if dir == 0.0 || y.len() < 2 {
        return 1.0;
    }
    let total = (y[y.len() - 1] - y[0]).abs();
    if total <= f64::EPSILON {
        return 1.0;
    }
    let mut worst = 0.0;
    if dir > 0.0 {
        let mut peak = y[0];
        for v in y {
            peak = peak.max(*v);
            let dd = peak - *v;
            if dd > worst {
                worst = dd;
            }
        }
    } else {
        let mut trough = y[0];
        for v in y {
            trough = trough.min(*v);
            let bounce = *v - trough;
            if bounce > worst {
                worst = bounce;
            }
        }
    }
    (worst / total).clamp(0.0, 2.0)
}

fn tail_move_share(y: &[f64], tail: usize) -> f64 {
    if y.len() < tail + 2 {
        return 0.0;
    }
    let total = (y[y.len() - 1] - y[0]).abs();
    if total <= f64::EPSILON {
        return 0.0;
    }
    let split = y.len() - tail - 1;
    let tail_move = (y[y.len() - 1] - y[split]).abs();
    (tail_move / total).clamp(0.0, 3.0)
}

fn mid_cross_count(y: &[f64]) -> usize {
    if y.len() < 2 {
        return 0;
    }
    let min = y.iter().fold(f64::INFINITY, |a, b| a.min(*b));
    let max = y.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b));
    let mid = (min + max) / 2.0;
    let mut prev = (y[0] - mid).signum();
    let mut crosses = 0;
    for v in &y[1..] {
        let sign = (*v - mid).signum();
        if sign != 0.0 && prev != 0.0 && sign != prev {
            crosses += 1;
        }
        if sign != 0.0 {
            prev = sign;
        }
    }
    crosses
}
