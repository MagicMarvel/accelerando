//! Cross detection.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossDir {
    /// `a` moved from at-or-below `b` to strictly above it.
    Above,
    /// `a` moved from at-or-above `b` to strictly below it.
    Below,
}

/// Detects when series `a` crosses series (or level) `b` between consecutive updates.
///
/// A cross needs a previous observation; the first update never reports one. Touching (`a == b`)
/// counts as the "before" side, so `prev <= 0 && now > 0` is an upward cross.
#[derive(Clone, Debug, Default)]
pub struct Cross {
    prev_diff: Option<f64>,
}

impl Cross {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, a: f64, b: f64) -> Option<CrossDir> {
        let diff = a - b;
        let out = match self.prev_diff {
            Some(prev) if prev <= 0.0 && diff > 0.0 => Some(CrossDir::Above),
            Some(prev) if prev >= 0.0 && diff < 0.0 => Some(CrossDir::Below),
            _ => None,
        };
        self.prev_diff = Some(diff);
        out
    }

    /// Last observed `a - b`.
    pub fn last_diff(&self) -> Option<f64> {
        self.prev_diff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_each_cross_once() {
        let mut c = Cross::new();
        assert_eq!(c.update(1.0, 2.0), None);
        assert_eq!(c.update(2.0, 2.0), None);
        assert_eq!(c.update(3.0, 2.0), Some(CrossDir::Above));
        assert_eq!(c.update(4.0, 2.0), None);
        assert_eq!(c.update(1.0, 2.0), Some(CrossDir::Below));
    }
}
