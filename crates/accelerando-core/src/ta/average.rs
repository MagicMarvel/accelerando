//! Moving averages and average true range.

use std::collections::VecDeque;

/// Exponential moving average with `alpha = 2 / (period + 1)`.
///
/// The first sample seeds the average (no SMA warm-up), which is the convention every
/// strategy in this ecosystem has used; check [`Ema::count`] if you need a warm-up gate.
#[derive(Clone, Debug)]
pub struct Ema {
    alpha: f64,
    value: Option<f64>,
    count: usize,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        Self {
            alpha: 2.0 / (period.max(1) as f64 + 1.0),
            value: None,
            count: 0,
        }
    }

    /// Fold in one sample and return the updated average.
    pub fn update(&mut self, x: f64) -> f64 {
        let next = match self.value {
            Some(prev) => prev + self.alpha * (x - prev),
            None => x,
        };
        self.value = Some(next);
        self.count += 1;
        next
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// Number of samples folded in so far.
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn reset(&mut self) {
        self.value = None;
        self.count = 0;
    }
}

impl Default for Ema {
    fn default() -> Self {
        Self::new(20)
    }
}

/// Simple moving average over the last `period` samples (including the current one).
#[derive(Clone, Debug, Default)]
pub struct Sma {
    period: usize,
    window: VecDeque<f64>,
    sum: f64,
}

impl Sma {
    pub fn new(period: usize) -> Self {
        Self {
            period: period.max(1),
            window: VecDeque::new(),
            sum: 0.0,
        }
    }

    /// Fold in one sample. Returns `None` until `period` samples have been seen.
    pub fn update(&mut self, x: f64) -> Option<f64> {
        self.window.push_back(x);
        self.sum += x;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or(0.0);
        }
        self.value()
    }

    pub fn value(&self) -> Option<f64> {
        (self.window.len() == self.period).then(|| self.sum / self.period as f64)
    }
}

/// Wilder average true range. The first `period` true ranges are averaged, then Wilder smoothing
/// (`atr = (atr * (n-1) + tr) / n`) takes over.
#[derive(Clone, Debug, Default)]
pub struct Atr {
    period: usize,
    prev_close: Option<f64>,
    seed_sum: f64,
    seen: usize,
    value: Option<f64>,
}

impl Atr {
    pub fn new(period: usize) -> Self {
        Self {
            period: period.max(1),
            ..Self::default()
        }
    }

    /// Fold in one bar. Returns `None` during the first `period - 1` bars.
    pub fn update(&mut self, high: f64, low: f64, close: f64) -> Option<f64> {
        let tr = match self.prev_close {
            Some(pc) => (high - low).max((high - pc).abs()).max((low - pc).abs()),
            None => high - low,
        };
        self.prev_close = Some(close);
        self.seen += 1;
        let n = self.period as f64;
        self.value = match self.value {
            Some(prev) => Some((prev * (n - 1.0) + tr) / n),
            None => {
                self.seed_sum += tr;
                (self.seen >= self.period).then(|| self.seed_sum / n)
            }
        };
        self.value
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_seeds_with_first_sample_and_converges() {
        let mut ema = Ema::new(3); // alpha 0.5
        assert_eq!(ema.update(10.0), 10.0);
        assert_eq!(ema.update(20.0), 15.0);
        assert_eq!(ema.update(20.0), 17.5);
        assert_eq!(ema.count(), 3);
    }

    #[test]
    fn sma_waits_for_a_full_window() {
        let mut sma = Sma::new(3);
        assert_eq!(sma.update(1.0), None);
        assert_eq!(sma.update(2.0), None);
        assert_eq!(sma.update(3.0), Some(2.0));
        assert_eq!(sma.update(6.0), Some(11.0 / 3.0));
    }

    #[test]
    fn atr_seeds_with_mean_then_wilder_smooths() {
        let mut atr = Atr::new(2);
        assert_eq!(atr.update(11.0, 9.0, 10.0), None); // tr 2
        assert_eq!(atr.update(12.0, 10.0, 11.0), Some(2.0)); // tr 2 -> mean 2
        // gap: tr = max(1, |15-11|, |14-11|) = 4 -> (2*1 + 4)/2 = 3
        assert_eq!(atr.update(15.0, 14.0, 14.5), Some(3.0));
    }
}
