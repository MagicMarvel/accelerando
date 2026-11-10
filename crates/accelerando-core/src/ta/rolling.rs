//! Rolling-window statistics.

use std::collections::VecDeque;

/// Rolling mean / sample standard deviation over the last `window` samples.
///
/// The usual pattern is [`RollingStats::zscore_then_push`]: score the current sample against the
/// *prior* window, then add it, so a sample never contributes to its own baseline.
#[derive(Clone, Debug, Default)]
pub struct RollingStats {
    window: usize,
    min_samples: usize,
    values: VecDeque<f64>,
    sum: f64,
    sum_sq: f64,
}

impl RollingStats {
    /// `min_samples` (default 10) is how many prior samples a z-score needs.
    pub fn new(window: usize) -> Self {
        let window = window.max(2);
        Self {
            window,
            min_samples: 10.min(window),
            values: VecDeque::new(),
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    pub fn with_min_samples(mut self, min_samples: usize) -> Self {
        self.min_samples = min_samples.max(2);
        self
    }

    pub fn push(&mut self, x: f64) {
        self.values.push_back(x);
        self.sum += x;
        self.sum_sq += x * x;
        while self.values.len() > self.window {
            if let Some(old) = self.values.pop_front() {
                self.sum -= old;
                self.sum_sq -= old * old;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn mean(&self) -> Option<f64> {
        (!self.values.is_empty()).then(|| self.sum / self.values.len() as f64)
    }

    /// Sample (n-1) standard deviation.
    pub fn std(&self) -> Option<f64> {
        let n = self.values.len();
        if n < 2 {
            return None;
        }
        let mean = self.sum / n as f64;
        Some(((self.sum_sq - self.sum * mean) / (n - 1) as f64).max(0.0).sqrt())
    }

    /// Z-score of `x` against the current window, without adding it. `None` when the window
    /// holds fewer than `min_samples` values or has (near) zero dispersion.
    pub fn zscore(&self, x: f64) -> Option<f64> {
        if self.values.len() < self.min_samples {
            return None;
        }
        let sd = self.std()?;
        let mean = self.mean()?;
        (sd > 1e-9).then(|| (x - mean) / sd)
    }

    /// Score `x` against the prior window, then add it to the window.
    pub fn zscore_then_push(&mut self, x: f64) -> Option<f64> {
        let z = self.zscore(x);
        self.push(x);
        z
    }
}

/// Highest / lowest value of the prior `window` samples (Donchian-style), O(1) amortized.
///
/// Call [`RollingExtrema::max`] / [`RollingExtrema::min`] *before* [`RollingExtrema::push`] to
/// compare the current bar against prior bars only.
#[derive(Clone, Debug, Default)]
pub struct RollingExtrema {
    window: usize,
    index: usize,
    maxq: VecDeque<(usize, f64)>,
    minq: VecDeque<(usize, f64)>,
}

impl RollingExtrema {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            ..Self::default()
        }
    }

    /// Add one sample (`high` feeds the max side, `low` the min side; pass the same value twice
    /// for a single series).
    pub fn push(&mut self, high: f64, low: f64) {
        let i = self.index;
        self.index += 1;
        while self.maxq.back().is_some_and(|&(_, v)| v <= high) {
            self.maxq.pop_back();
        }
        self.maxq.push_back((i, high));
        while self.minq.back().is_some_and(|&(_, v)| v >= low) {
            self.minq.pop_back();
        }
        self.minq.push_back((i, low));
        let oldest = self.index.saturating_sub(self.window);
        while self.maxq.front().is_some_and(|&(j, _)| j < oldest) {
            self.maxq.pop_front();
        }
        while self.minq.front().is_some_and(|&(j, _)| j < oldest) {
            self.minq.pop_front();
        }
    }

    /// `true` once `window` samples have been pushed.
    pub fn is_full(&self) -> bool {
        self.index >= self.window
    }

    pub fn max(&self) -> Option<f64> {
        self.maxq.front().map(|&(_, v)| v)
    }

    pub fn min(&self) -> Option<f64> {
        self.minq.front().map(|&(_, v)| v)
    }
}

/// Fixed-capacity history: `ago(0)` is the most recent value pushed, `ago(1)` the one before.
#[derive(Clone, Debug, Default)]
pub struct Lookback<T> {
    capacity: usize,
    values: VecDeque<T>,
}

impl<T: Copy> Lookback<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            values: VecDeque::new(),
        }
    }

    pub fn push(&mut self, value: T) {
        self.values.push_back(value);
        while self.values.len() > self.capacity {
            self.values.pop_front();
        }
    }

    pub fn ago(&self, n: usize) -> Option<T> {
        let len = self.values.len();
        (n < len).then(|| self.values[len - 1 - n])
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.values.len() == self.capacity
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.values.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zscore_uses_prior_window_only() {
        let mut s = RollingStats::new(100).with_min_samples(3);
        assert_eq!(s.zscore_then_push(1.0), None);
        assert_eq!(s.zscore_then_push(2.0), None);
        assert_eq!(s.zscore_then_push(3.0), None);
        // prior window [1,2,3]: mean 2, sample sd 1
        assert_eq!(s.zscore_then_push(4.0), Some(2.0));
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn rolling_stats_drops_old_values() {
        let mut s = RollingStats::new(2);
        s.push(100.0);
        s.push(1.0);
        s.push(3.0);
        assert_eq!(s.mean(), Some(2.0));
    }

    #[test]
    fn extrema_track_the_prior_window() {
        let mut e = RollingExtrema::new(3);
        for (h, l) in [(5.0, 1.0), (7.0, 2.0), (6.0, 3.0)] {
            e.push(h, l);
        }
        assert_eq!((e.max(), e.min()), (Some(7.0), Some(1.0)));
        e.push(4.0, 4.0); // 5/1 drops out
        assert_eq!((e.max(), e.min()), (Some(7.0), Some(2.0)));
        e.push(4.0, 4.0);
        e.push(4.0, 4.0);
        assert_eq!((e.max(), e.min()), (Some(4.0), Some(4.0)));
    }

    #[test]
    fn lookback_indexes_from_most_recent() {
        let mut lb = Lookback::new(2);
        lb.push(1);
        lb.push(2);
        lb.push(3);
        assert_eq!((lb.ago(0), lb.ago(1), lb.ago(2)), (Some(3), Some(2), None));
    }
}
