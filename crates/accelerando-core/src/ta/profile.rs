//! Tick-keyed volume profile and value area.

use std::collections::BTreeMap;

use crate::footprint::{Footprint, VpLevel};

/// POC / value-area bounds of a profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueArea {
    pub poc: f64,
    pub val: f64,
    pub vah: f64,
}

impl ValueArea {
    pub fn width(&self) -> f64 {
        self.vah - self.val
    }

    pub fn contains(&self, price: f64) -> bool {
        price >= self.val && price <= self.vah
    }
}

/// Volume profile keyed by integer tick (`round(price / tick)`), so prices that differ only by
/// floating-point noise land on the same row.
///
/// Supports both growing profiles (session / anchored) and rolling ones via
/// [`VolumeProfile::remove_footprint`].
#[derive(Clone, Debug, Default)]
pub struct VolumeProfile {
    tick: f64,
    levels: BTreeMap<i64, (f64, f64)>,
    total: f64,
}

impl VolumeProfile {
    pub fn new(tick: f64) -> Self {
        Self {
            tick: if tick > 0.0 { tick } else { 0.25 },
            levels: BTreeMap::new(),
            total: 0.0,
        }
    }

    pub fn tick(&self) -> f64 {
        self.tick
    }

    fn key(&self, price: f64) -> i64 {
        (price / self.tick).round() as i64
    }

    /// Add buy/sell volume at `price` (negative values subtract; empty rows are dropped).
    pub fn add(&mut self, price: f64, buy: f64, sell: f64) {
        let key = self.key(price);
        let row = self.levels.entry(key).or_insert((0.0, 0.0));
        row.0 += buy;
        row.1 += sell;
        self.total += buy + sell;
        if row.0 + row.1 <= 1e-9 {
            self.levels.remove(&key);
        }
    }

    /// Add every ladder level of a footprint.
    pub fn add_footprint(&mut self, fp: &Footprint) {
        for level in &fp.ladder {
            self.add(level.price, level.buy_vol, level.sell_vol);
        }
    }

    /// Subtract a footprint previously added (rolling-window profiles).
    pub fn remove_footprint(&mut self, fp: &Footprint) {
        for level in &fp.ladder {
            self.add(level.price, -level.buy_vol, -level.sell_vol);
        }
    }

    pub fn clear(&mut self) {
        self.levels.clear();
        self.total = 0.0;
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub fn total_volume(&self) -> f64 {
        self.total
    }

    pub fn volume_at(&self, price: f64) -> f64 {
        self.levels
            .get(&self.key(price))
            .map(|(b, s)| b + s)
            .unwrap_or(0.0)
    }

    /// Highest-volume price (ties resolve to the higher price).
    pub fn poc(&self) -> Option<f64> {
        self.levels
            .iter()
            .max_by(|a, b| (a.1 .0 + a.1 .1).total_cmp(&(b.1 .0 + b.1 .1)))
            .map(|(&k, _)| k as f64 * self.tick)
    }

    /// Standard value area: start at the POC and repeatedly extend one tick toward the larger
    /// adjacent row (ties extend upward; empty ticks inside the profile count as zero) until
    /// `fraction` of the volume is covered.
    pub fn value_area(&self, fraction: f64) -> Option<ValueArea> {
        let total: f64 = self.levels.values().map(|(b, s)| b + s).sum();
        if total <= 0.0 {
            return None;
        }
        let vol = |k: i64| self.levels.get(&k).map(|(b, s)| b + s).unwrap_or(0.0);
        let (&poc, _) = self
            .levels
            .iter()
            .max_by(|a, b| (a.1 .0 + a.1 .1).total_cmp(&(b.1 .0 + b.1 .1)))?;
        let (&min_k, _) = self.levels.iter().next()?;
        let (&max_k, _) = self.levels.iter().next_back()?;
        let (mut lo, mut hi) = (poc, poc);
        let mut acc = vol(poc);
        while acc < fraction * total && (lo > min_k || hi < max_k) {
            let up = if hi < max_k { vol(hi + 1) } else { -1.0 };
            let down = if lo > min_k { vol(lo - 1) } else { -1.0 };
            if up >= down {
                hi += 1;
                acc += up;
            } else {
                lo -= 1;
                acc += down;
            }
        }
        Some(ValueArea {
            poc: poc as f64 * self.tick,
            val: lo as f64 * self.tick,
            vah: hi as f64 * self.tick,
        })
    }

    /// Rows in ascending price order, ready for `Plot`/chart output.
    pub fn levels(&self) -> Vec<VpLevel> {
        self.levels
            .iter()
            .map(|(&k, &(buy_vol, sell_vol))| VpLevel {
                price: k as f64 * self.tick,
                buy_vol,
                sell_vol,
            })
            .collect()
    }

    /// `(low, high)` price of the profile.
    pub fn range(&self) -> Option<(f64, f64)> {
        let lo = *self.levels.keys().next()?;
        let hi = *self.levels.keys().next_back()?;
        Some((lo as f64 * self.tick, hi as f64 * self.tick))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(rows: &[(f64, f64)]) -> VolumeProfile {
        let mut p = VolumeProfile::new(0.25);
        for &(price, vol) in rows {
            p.add(price, vol, 0.0);
        }
        p
    }

    #[test]
    fn value_area_expands_toward_larger_neighbour() {
        let p = profile(&[(99.5, 5.0), (99.75, 10.0), (100.0, 40.0), (100.25, 30.0), (100.5, 15.0)]);
        let va = p.value_area(0.70).unwrap();
        assert_eq!(va.poc, 100.0);
        // 40 + 30 (up) = 70 of 100
        assert_eq!((va.val, va.vah), (100.0, 100.25));
    }

    #[test]
    fn float_noise_lands_on_one_row_and_rolling_removal_works() {
        let mut p = VolumeProfile::new(0.25);
        let mut fp = Footprint::seed(0, 100.0);
        fp.add_trade(0, 100.000000001, 3.0, true, 0.25);
        p.add_footprint(&fp);
        p.add(100.0, 0.0, 2.0);
        assert_eq!(p.levels().len(), 1);
        assert_eq!(p.volume_at(100.0), 5.0);
        p.remove_footprint(&fp);
        assert_eq!(p.volume_at(100.0), 2.0);
    }

    #[test]
    fn value_area_walks_across_empty_ticks() {
        let p = profile(&[(100.0, 50.0), (101.0, 50.0)]);
        let va = p.value_area(0.9).unwrap();
        assert_eq!((va.val, va.vah), (100.0, 101.0));
    }
}
