//! Sample the visible time range directly from the event tape and sparse depth.

use std::sync::Arc;

use accelerando_core::{DepthHistory, OrderFlowEvent, PreparedEventData, Side};

use crate::data::{
    BookGrid, HeatMetric, QuoteColumn, RasterSpec, Segment, TimeAxis, TradeBubble, TradeQuote,
    SEC_NS,
};

pub(crate) struct TradeTape {
    data: Arc<PreparedEventData>,
    first: usize,
    index: Option<Vec<usize>>,
    last_per_second: [Vec<i64>; 2],
}

impl TradeTape {
    pub fn new(data: Arc<PreparedEventData>, quotes: &[TradeQuote]) -> Self {
        let first = data
            .events()
            .iter()
            .position(is_trade)
            .unwrap_or(data.events().len());
        let index = data.events()[first..]
            .iter()
            .any(|event| !is_trade(event))
            .then(|| {
                data.events()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, event)| is_trade(event).then_some(i))
                    .collect()
            });
        let last_per_second = std::array::from_fn(|side| {
            quotes
                .iter()
                .filter_map(|quote| quote.last_trade_ns[side])
                .collect()
        });
        Self {
            data,
            first,
            index,
            last_per_second,
        }
    }

    fn len(&self) -> usize {
        self.index
            .as_ref()
            .map_or(self.data.events().len() - self.first, Vec::len)
    }

    fn get(&self, i: usize) -> (i64, f64, f64, Side) {
        let index = self
            .index
            .as_ref()
            .map_or_else(|| self.first + i, |indices| indices[i]);
        match self.data.events()[index] {
            OrderFlowEvent::Trade {
                ts_ns,
                price,
                size,
                aggressor,
            } => (ts_ns, price, size, aggressor),
            _ => unreachable!("trade index only contains trades"),
        }
    }

    fn carried_quotes(&self, seg: &Segment, start: i64, width_ns: i64) -> [f64; 2] {
        let seed = start.div_euclid(SEC_NS) * SEC_NS;
        let mut last = self.last_per_second.each_ref().map(|times| {
            let index = times.partition_point(|&ts| ts < seed);
            index
                .checked_sub(1)
                .map(|i| times[i])
                .filter(|&ts| ts >= seg.t0_ns)
        });
        let mut i = self.lower_bound(seed);
        while i < self.len() {
            let (ts, _, _, side) = self.get(i);
            if ts >= start {
                break;
            }
            if let Some(side) = side_index(side) {
                last[side] = Some(ts);
            }
            i += 1;
        }
        std::array::from_fn(|side| {
            let Some(ts) = last[side] else {
                return f64::NAN;
            };
            let bin = seg.t0_ns + (ts - seg.t0_ns).div_euclid(width_ns) * width_ns;
            let until = bin.saturating_add(width_ns).min(start);
            let mut i = self.lower_bound(bin);
            let mut value = f64::NAN;
            while i < self.len() {
                let (ts, price, _, aggressor) = self.get(i);
                if ts >= until {
                    break;
                }
                if side_index(aggressor) == Some(side) {
                    value = if side == 0 {
                        value.min(price)
                    } else {
                        value.max(price)
                    };
                }
                i += 1;
            }
            value
        })
    }

    fn lower_bound(&self, ts: i64) -> usize {
        let (mut start, mut end) = (0, self.len());
        while start < end {
            let mid = start + (end - start) / 2;
            if self.get(mid).0 < ts {
                start = mid + 1;
            } else {
                end = mid;
            }
        }
        start
    }
}

fn side_index(side: Side) -> Option<usize> {
    match side {
        Side::Buy => Some(1),
        Side::Sell => Some(0),
        Side::Unknown => None,
    }
}

fn is_trade(event: &OrderFlowEvent) -> bool {
    matches!(event, OrderFlowEvent::Trade { .. })
}

fn at_x(segment: &Segment, x: f64) -> i64 {
    segment.t0_ns + ((x - segment.x0) * SEC_NS as f64).round() as i64
}

fn at_ts(segment: &Segment, ts: i64) -> f64 {
    segment.x0 + (ts - segment.t0_ns) as f64 / SEC_NS as f64
}

pub(crate) fn book_quote(depth: &DepthHistory, ts: i64) -> [f64; 2] {
    let changes = depth.quote_changes();
    let end = changes.partition_point(|change| change.0 <= ts);
    end.checked_sub(1).map_or([f64::NAN; 2], |i| changes[i].1)
}

/// Cache only the quote overview. The heatmap and close zoom still query the raw history.
pub(crate) fn fill_overview_quotes(grid: &mut BookGrid, axis: &TimeAxis, depth: &DepthHistory) {
    let changes = depth.quote_changes();
    let mut next = 0;
    for seg in &axis.segments {
        let first = seg.x0 as usize;
        for offset in 0..seg.len_secs() as usize {
            let until = seg.t0_ns + (offset as i64 + 1) * SEC_NS;
            while next < changes.len() && changes[next].0 < until {
                next += 1;
            }
            let prices = next.checked_sub(1).map_or([f64::NAN; 2], |i| changes[i].1);
            grid.bid[first + offset] = prices[0] as f32;
            grid.ask[first + offset] = prices[1] as f32;
        }
    }
}

pub(crate) struct Viewport {
    pub start: f64,
    pub end: f64,
    pub pixels: f64,
    pub replay_ns: Option<i64>,
}

/// Use one column per screen pixel, rounded up to a nanosecond. Trade-only quotes use
/// aggressive buy highs and sell lows in the column, carrying each side through empty columns.
pub(crate) fn fine_columns(
    axis: &TimeAxis,
    tape: Option<&TradeTape>,
    depth: &DepthHistory,
    view: &Viewport,
    show_bubbles: bool,
) -> Vec<Vec<QuoteColumn>> {
    let width_ns = (((view.end - view.start) / view.pixels) * SEC_NS as f64)
        .ceil()
        .max(1.0) as i64;
    let first = axis
        .segments
        .partition_point(|seg| seg.x0 + seg.len_secs() <= view.start);
    axis.segments[first..]
        .iter()
        .take_while(|seg| seg.x0 < view.end)
        .filter_map(|seg| {
            let visible_start = at_x(seg, view.start.max(seg.x0));
            let end_x = view.end.min(seg.x0 + seg.len_secs());
            let end =
                at_x(seg, end_x).min(view.replay_ns.map_or(i64::MAX, |ts| ts.saturating_add(1)));
            let start = seg.t0_ns + (visible_start - seg.t0_ns).div_euclid(width_ns) * width_ns;
            if end <= start {
                return None;
            }

            let mut last = if depth.is_empty() {
                tape.map_or([f64::NAN; 2], |tape| {
                    tape.carried_quotes(seg, start, width_ns)
                })
            } else {
                [f64::NAN; 2]
            };
            let mut trade_index = tape.map_or(0, |tape| tape.lower_bound(start));

            let mut columns = Vec::new();
            let mut from = start;
            while from < end {
                let until = from.saturating_add(width_ns).min(end);
                let mut prices = [f64::NAN; 2];
                let (mut buy, mut sell, mut notional, mut time) = (0.0, 0.0, 0.0, 0.0);
                if let Some(tape) = tape.filter(|_| show_bubbles || depth.is_empty()) {
                    while trade_index < tape.len() {
                        let (ts, price, size, side) = tape.get(trade_index);
                        if ts >= until {
                            break;
                        }
                        trade_index += 1;
                        match side {
                            Side::Buy => {
                                prices[1] = prices[1].max(price);
                                buy += size;
                            }
                            Side::Sell => {
                                prices[0] = prices[0].min(price);
                                sell += size;
                            }
                            Side::Unknown => continue,
                        }
                        notional += price * size;
                        time += (ts - from) as f64 * size;
                    }
                }
                if depth.is_empty() {
                    for (previous, price) in last.iter_mut().zip(prices) {
                        if price.is_finite() {
                            *previous = price;
                        }
                    }
                } else {
                    last = book_quote(depth, until - 1);
                }
                let total = buy + sell;
                columns.push(QuoteColumn {
                    x: (at_ts(seg, from) + at_ts(seg, until)) * 0.5,
                    prices: last,
                    bubble: (show_bubbles && total > 0.0).then(|| TradeBubble {
                        x: at_ts(seg, from) + time / total / SEC_NS as f64,
                        price: notional / total,
                        buy,
                        sell,
                    }),
                });
                from = until;
            }
            Some(columns)
        })
        .collect()
}

/// Integrate resting sizes over each pixel's time span, including exact depth clears.
pub(crate) fn rasterize_depth(
    spec: &RasterSpec,
    axis: &TimeAxis,
    depth: &DepthHistory,
    metric: HeatMetric,
    replay_ns: Option<i64>,
) -> Vec<f32> {
    let mut cells = vec![0.0_f32; spec.cols * spec.rows];
    let (top, bottom) = spec.price_range();
    let times: Vec<_> = (0..spec.cols)
        .map(|col| {
            let lo = (spec.c_lo + col as i64) as f64 * spec.col_w;
            let hi = lo + spec.col_w;
            let i = axis
                .segments
                .partition_point(|seg| seg.x0 < hi)
                .checked_sub(1)?;
            let seg = &axis.segments[i];
            let seg_end = seg.x0 + seg.len_secs();
            if lo >= seg_end || hi <= seg.x0 {
                return None;
            }
            let from = at_x(seg, lo.max(seg.x0));
            let until = at_x(seg, hi.min(seg_end))
                .min(replay_ns.map_or(i64::MAX, |ts| ts.saturating_add(1)));
            (from < until).then_some((from, until))
        })
        .collect();
    for (price, level) in depth.levels_between(bottom, top) {
        let row = spec.k_hi - (price / spec.row_p).round() as i64;
        if !(0..spec.rows as i64).contains(&row) {
            continue;
        }
        let row = &mut cells[row as usize * spec.cols..][..spec.cols];
        for (cell, &interval) in row.iter_mut().zip(&times) {
            let Some((from, until)) = interval else {
                continue;
            };
            let [bid, ask] = level.mean_size(from, until);
            let size = match metric {
                HeatMetric::Total => bid + ask,
                HeatMetric::Bid => bid,
                HeatMetric::Ask => ask,
            };
            *cell = cell.max(size as f32);
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_uses_nanosecond_book_updates_and_replay_cutoff() {
        struct Source(Vec<OrderFlowEvent>);
        impl accelerando_core::DataSource for Source {
            fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
                Box::new(self.0.into_iter())
            }
        }
        let update = |ts_ns, size| OrderFlowEvent::SetLevel {
            ts_ns,
            size,
            price: 100.0,
            side: Side::Buy,
        };
        let (_, depth) = accelerando_core::prepare_market_history(Box::new(Source(vec![
            update(100, 5.0),
            update(105, 7.0),
            update(110, 9.0),
            update(120, 0.0),
        ])));
        let axis = TimeAxis::build(vec![(100, 130)], SEC_NS);
        let spec = RasterSpec {
            c_lo: 10,
            cols: 3,
            col_w: 10e-9,
            k_hi: 400,
            rows: 1,
            row_p: 0.25,
        };
        assert_eq!(
            rasterize_depth(&spec, &axis, &depth, HeatMetric::Bid, None),
            vec![6.0, 9.0, 0.0]
        );
        assert_eq!(
            rasterize_depth(&spec, &axis, &depth, HeatMetric::Bid, Some(105)),
            vec![16.0 / 3.0, 0.0, 0.0]
        );
    }

    #[test]
    fn subsecond_columns_keep_exact_trade_time_and_hide_future_replay_volume() {
        let events = vec![
            OrderFlowEvent::Contract {
                tick_size: 0.25,
                multiplier: 50.0,
            },
            OrderFlowEvent::Trade {
                ts_ns: 101_000_000,
                price: 100.25,
                size: 2.0,
                aggressor: Side::Buy,
            },
            OrderFlowEvent::Trade {
                ts_ns: 108_000_000,
                price: 100.0,
                size: 3.0,
                aggressor: Side::Sell,
            },
            OrderFlowEvent::Trade {
                ts_ns: 115_000_000,
                price: 100.50,
                size: 4.0,
                aggressor: Side::Buy,
            },
        ];
        let quotes = crate::data::trade_quotes(&events);
        let tape = TradeTape::new(
            Arc::new(PreparedEventData::from_events(events).unwrap()),
            &quotes,
        );
        let axis = TimeAxis::build(vec![(0, 200_000_000)], SEC_NS);
        let view = Viewport {
            start: 0.1,
            end: 0.12,
            pixels: 20.0,
            replay_ns: Some(110_000_000),
        };
        let columns = fine_columns(&axis, Some(&tape), &DepthHistory::default(), &view, true);
        let bubbles: Vec<_> = columns
            .iter()
            .flatten()
            .filter_map(|col| col.bubble)
            .collect();
        assert_eq!(bubbles.len(), 2);
        assert!((bubbles[0].x - 0.101).abs() < 1e-12);
        assert!((bubbles[1].x - 0.108).abs() < 1e-12);
        assert_eq!(bubbles[1].sell, 3.0);
        assert_eq!(columns[0].last().unwrap().prices, [100.0, 100.25]);
    }
}
