//! Sparse, event-time order-book history for display at arbitrary zoom levels.

use std::collections::{BTreeMap, BTreeSet};

use crate::{DataSource, EventInterest, OrderFlowEvent, PreparedEventData, Side};

type Prices = [f64; 2];
pub type DepthChange = (i64, [f64; 2]);

/// One time series per price, plus a best-bid/ask series. No samples are generated between events.
pub struct DepthHistory {
    tick: f64,
    levels: BTreeMap<i64, DepthLevelHistory>,
    quotes: Vec<(i64, Prices)>,
}

impl Default for DepthHistory {
    fn default() -> Self {
        Self {
            tick: 0.25,
            levels: BTreeMap::new(),
            quotes: Vec::new(),
        }
    }
}

impl DepthHistory {
    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub fn quote_changes(&self) -> &[(i64, [f64; 2])] {
        &self.quotes
    }

    pub fn levels_between(
        &self,
        low: f64,
        high: f64,
    ) -> impl Iterator<Item = (f64, &DepthLevelHistory)> {
        let first = (low / self.tick).floor() as i64;
        let last = (high / self.tick).ceil() as i64;
        self.levels
            .range(first..=last)
            .map(|(&key, level)| (key as f64 * self.tick, level))
    }
}

/// Changes and sparse integrals of resting size at one price. The integral checkpoints
/// keep wide-pixel queries bounded without storing another full series of market updates.
#[derive(Default)]
pub struct DepthLevelHistory {
    changes: Vec<DepthChange>,
    integrals: Vec<[f64; 2]>,
}

const INTEGRAL_BLOCK: usize = 64;

impl DepthLevelHistory {
    pub fn changes(&self) -> &[DepthChange] {
        &self.changes
    }

    fn push(&mut self, ts: i64, size: [f64; 2]) {
        if self.changes.last().is_some_and(|point| point.0 == ts) {
            self.changes.last_mut().unwrap().1 = size;
            return;
        }
        self.changes.push((ts, size));
        let index = self.changes.len() - 1;
        if index % INTEGRAL_BLOCK == 0 {
            let mut integral = self.integrals.last().copied().unwrap_or_default();
            if index > 0 {
                self.add_intervals(&mut integral, index - INTEGRAL_BLOCK, index);
            }
            self.integrals.push(integral);
        }
    }

    fn add_intervals(&self, area: &mut [f64; 2], from: usize, until: usize) {
        for pair in self.changes[from..=until].windows(2) {
            add_area(area, pair[0].1, pair[1].0 - pair[0].0);
        }
    }

    /// Time-weighted resting bid and ask size over `[from, until)`.
    pub fn mean_size(&self, from: i64, until: i64) -> [f64; 2] {
        if from >= until {
            return [0.0; 2];
        }
        let end = self.changes.partition_point(|point| point.0 < until);
        let Some(last) = end.checked_sub(1) else {
            return [0.0; 2];
        };
        let first = self
            .changes
            .partition_point(|point| point.0 <= from)
            .saturating_sub(1);
        let start = from.max(self.changes[first].0);
        let mut area = [0.0; 2];
        if first == last {
            add_area(&mut area, self.changes[first].1, until - start);
        } else {
            add_area(
                &mut area,
                self.changes[first].1,
                self.changes[first + 1].0 - start,
            );
            let mut index = first + 1;
            let block_start = index.div_ceil(INTEGRAL_BLOCK) * INTEGRAL_BLOCK;
            let block_end = (last / INTEGRAL_BLOCK) * INTEGRAL_BLOCK;
            if block_start < block_end {
                self.add_intervals(&mut area, index, block_start);
                for (side, area) in area.iter_mut().enumerate() {
                    *area += self.integrals[block_end / INTEGRAL_BLOCK][side]
                        - self.integrals[block_start / INTEGRAL_BLOCK][side];
                }
                index = block_end;
            }
            self.add_intervals(&mut area, index, last);
            add_area(
                &mut area,
                self.changes[last].1,
                until - self.changes[last].0,
            );
        }
        area.map(|area| area / (until - from) as f64)
    }
}

fn add_area(area: &mut [f64; 2], size: [f64; 2], duration_ns: i64) {
    for (area, size) in area.iter_mut().zip(size) {
        *area += size * duration_ns as f64;
    }
}

/// Build a native viewer's trade tape and event-time depth in one pass through the data source.
pub fn prepare_market_history(
    mut source: Box<dyn DataSource>,
) -> (Result<PreparedEventData, String>, DepthHistory) {
    source.set_event_interest(EventInterest::ALL);
    let mut trades = Vec::new();
    let mut depth = DepthBuilder::default();
    for event in source.events() {
        match event {
            OrderFlowEvent::Contract { .. } => {
                depth.update(event);
                trades.push(event);
            }
            OrderFlowEvent::Trade { .. } => trades.push(event),
            _ => depth.update(event),
        }
    }
    (PreparedEventData::from_events(trades), depth.history)
}

#[derive(Default)]
struct DepthBuilder {
    history: DepthHistory,
    active: [BTreeSet<i64>; 2],
}

impl DepthBuilder {
    fn update(&mut self, event: OrderFlowEvent) {
        let (ts, price, size, side, absolute) = match event {
            OrderFlowEvent::Contract { tick_size, .. } => {
                if self.history.is_empty() && tick_size.is_finite() && tick_size > 0.0 {
                    self.history.tick = tick_size;
                }
                return;
            }
            OrderFlowEvent::SetLevel {
                ts_ns,
                price,
                size,
                side,
            } => (ts_ns, price, size, side, true),
            OrderFlowEvent::AddLimit {
                ts_ns,
                price,
                size,
                side,
            } => (ts_ns, price, size, side, false),
            OrderFlowEvent::ReduceLimit {
                ts_ns,
                price,
                size,
                side,
            } => (ts_ns, price, -size, side, false),
            OrderFlowEvent::Trade { .. } => return,
        };
        let side = match side {
            Side::Buy => 0,
            Side::Sell => 1,
            Side::Unknown => return,
        };
        if !price.is_finite() || !size.is_finite() {
            return;
        }
        let tick = self.history.tick;
        let key = (price / tick).round() as i64;
        let levels = self.history.levels.entry(key).or_default();
        let previous = levels.changes().last().map_or([0.0; 2], |change| change.1);
        let mut current = previous;
        current[side] = if absolute {
            size
        } else {
            previous[side] + size
        }
        .max(0.0);
        if current == previous {
            if levels.changes().is_empty() {
                self.history.levels.remove(&key);
            }
            return;
        }
        levels.push(ts, current);
        if current[side] > 0.0 {
            self.active[side].insert(key);
        } else {
            self.active[side].remove(&key);
        }
        let prices = [
            self.active[0].last().map_or(f64::NAN, |&k| k as f64 * tick),
            self.active[1]
                .first()
                .map_or(f64::NAN, |&k| k as f64 * tick),
        ];
        if self.history.quotes.last().is_none_or(|(_, prev)| {
            prev.iter()
                .zip(prices)
                .any(|(&a, b)| a.to_bits() != b.to_bits())
        }) {
            append_at_time(&mut self.history.quotes, ts, prices);
        }
    }
}

fn append_at_time<T>(points: &mut Vec<(i64, T)>, ts: i64, value: T) {
    if let Some(last) = points.last_mut().filter(|point| point.0 == ts) {
        last.1 = value;
    } else {
        points.push((ts, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_integrates_short_and_wide_windows_without_losing_clears() {
        let mut level = DepthLevelHistory::default();
        for ts in 0..300 {
            level.push(ts, [if ts % 2 == 0 { 8.0 } else { 0.0 }, 2.0]);
        }
        level.push(299, [0.0, 4.0]);
        assert_eq!(level.mean_size(1, 2), [0.0, 2.0]);
        assert_eq!(level.mean_size(0, 298), [4.0, 2.0]);
        assert_eq!(level.mean_size(298, 300), [4.0, 3.0]);
        assert_eq!(level.mean_size(-1, 1), [4.0, 1.0]);
    }

    #[test]
    fn keeps_subsecond_depth_changes_and_best_price_clears() {
        let mut builder = DepthBuilder::default();
        for (ts_ns, price, size, side) in [
            (100, 100.0, 2.0, Side::Buy),
            (110, 100.25, 3.0, Side::Sell),
            (120, 100.0, 5.0, Side::Buy),
            (120, 100.0, 6.0, Side::Buy),
            (130, 100.0, 0.0, Side::Buy),
        ] {
            builder.update(OrderFlowEvent::SetLevel {
                ts_ns,
                price,
                size,
                side,
            });
        }
        let history = builder.history;
        let (_, level) = history.levels_between(100.0, 100.0).next().unwrap();
        assert_eq!(
            level.changes(),
            &[(100, [2.0, 0.0]), (120, [6.0, 0.0]), (130, [0.0, 0.0])]
        );
        assert_eq!(history.quote_changes()[1], (110, [100.0, 100.25]));
        assert!(history.quote_changes()[2].1[0].is_nan());
    }
}
