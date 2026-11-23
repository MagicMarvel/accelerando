//! Price-level studies for aggregated depth (market by price).

use std::collections::HashMap;

use accelerando_core::{depth_history::DepthChange, DepthHistory, Side};

use crate::data::{HeatMetric, RasterSpec, SEC_NS};

const TRADE_MATCH_NS: i64 = 10_000_000;
const FLOW_WINDOW_NS: i64 = 5 * SEC_NS;
pub(crate) const FLOW_LOOKBACK_NS: i64 = FLOW_WINDOW_NS + TRADE_MATCH_NS;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FlowMarker {
    pub study: Study,
    pub ts: i64,
    pub price: f64,
    pub volume: f64,
    pub resting: Option<[f64; 2]>,
}

pub(crate) const MARKER_STUDIES: [Study; 3] = [Study::Added, Study::Decreased, Study::Cancelled];

pub(crate) struct FlowQuery {
    pub prices: (f64, f64),
    pub marker_prices: (f64, f64),
    pub visible_times: (i64, i64),
    pub read_until: i64,
    pub initial_depth_ns: Option<i64>,
    pub metric: HeatMetric,
    pub tick: f64,
    pub markers: [Option<u32>; 3],
    pub marker_limit: usize,
}

#[derive(Default)]
pub(crate) struct FlowLayers {
    pub markers: Vec<FlowMarker>,
    pub net: Vec<f32>,
}

pub(crate) fn flow_layers(
    depth: &DepthHistory,
    query: &FlowQuery,
    net: Option<(&RasterSpec, &[Option<(i64, i64)>])>,
    trades: impl Iterator<Item = (i64, f64, f64, Side)>,
) -> FlowLayers {
    let columns = net.map_or_else(Vec::new, |(_, times)| visible_columns(times));
    let mut result = FlowLayers {
        net: net.map_or_else(Vec::new, |(spec, _)| vec![0.0; spec.rows * spec.cols]),
        ..FlowLayers::default()
    };
    let from = columns.first().map_or(query.visible_times.0, |column| {
        column.1.min(query.visible_times.0)
    });
    let start = window_start(from);
    if start >= query.read_until {
        return result;
    }

    let (low, high) = query.prices;
    let price_key = |price: f64| (price / query.tick).round() as i64;
    let mut prints = HashMap::<i64, [Vec<(i64, f64)>; 2]>::new();
    for (ts, price, size, aggressor) in trades {
        let side = match aggressor {
            Side::Sell => 0,
            Side::Buy => 1,
            Side::Unknown => continue,
        };
        if (low..=high).contains(&price) && size.is_finite() && size > 0.0 {
            prints.entry(price_key(price)).or_default()[side].push((ts, size));
        }
    }
    let match_trades = net.is_some() || query.markers[2].is_some();
    let mut markers: [Vec<FlowMarker>; 3] = std::array::from_fn(|_| Vec::new());
    let strongest_first = |a: &FlowMarker, b: &FlowMarker| {
        b.volume
            .total_cmp(&a.volume)
            .then(a.ts.cmp(&b.ts))
            .then(a.price.total_cmp(&b.price))
    };
    for (price, level) in depth
        .levels_between(low, high)
        .filter(|(price, _)| (low..=high).contains(price))
    {
        let net_row = net.and_then(|(spec, _)| {
            let row = spec.k_hi - (price / spec.row_p).round() as i64;
            (0..spec.rows as i64)
                .contains(&row)
                .then(|| &mut result.net[row as usize * spec.cols..][..spec.cols])
        });
        let mut net_row = net_row;
        let mut collect = |bucket: FlowBucket| {
            if let Some(row) = net_row.as_deref_mut() {
                rasterize_net_bucket(row, &columns, &bucket);
            }
            for (index, threshold) in query.markers.iter().enumerate() {
                let Some(minimum) = threshold else { continue };
                let (volume, ts, resting) = if index < 2 {
                    let Some(change) = bucket
                        .resting_change
                        .filter(|_| bucket.from.saturating_add(FLOW_WINDOW_NS) <= query.read_until)
                    else {
                        continue;
                    };
                    let Some(ts) = change.crossed_at[index] else {
                        continue;
                    };
                    (
                        (change.end - change.start).abs(),
                        ts,
                        Some([change.start, change.end]),
                    )
                } else {
                    (bucket.volumes[index], bucket.last_cancel, None)
                };
                if volume < f64::from(*minimum)
                    || ts < query.visible_times.0
                    || ts >= query.visible_times.1
                    || !(query.marker_prices.0..=query.marker_prices.1).contains(&price)
                {
                    continue;
                }
                let layer = &mut markers[index];
                layer.push(FlowMarker {
                    study: MARKER_STUDIES[index],
                    ts,
                    price,
                    volume,
                    resting,
                });
                if layer.len() == 2 * query.marker_limit {
                    layer.select_nth_unstable_by(query.marker_limit, strongest_first);
                    layer.truncate(query.marker_limit);
                }
            }
        };
        let level_trades = prints.remove(&price_key(price)).unwrap_or_default();
        flow_buckets(
            level.changes(),
            start,
            query,
            match_trades,
            level_trades,
            &mut collect,
        );
    }
    for mut layer in markers {
        layer.sort_unstable_by(strongest_first);
        layer.truncate(query.marker_limit);
        result.markers.extend(layer);
    }
    result.markers.sort_unstable_by(strongest_first);
    result
}

fn window_start(ts: i64) -> i64 {
    ts.div_euclid(FLOW_WINDOW_NS) * FLOW_WINDOW_NS
}

pub(crate) fn flow_read_until(until: i64) -> i64 {
    window_start(until.saturating_sub(1)).saturating_add(FLOW_WINDOW_NS)
}

#[derive(Debug, Default)]
struct FlowBucket {
    from: i64,
    volumes: [f64; 3],
    last_cancel: i64,
    resting_change: Option<RestingChange>,
}

#[derive(Clone, Copy, Debug)]
struct RestingChange {
    start: f64,
    end: f64,
    crossed_at: [Option<i64>; 2],
}

impl RestingChange {
    fn update(&mut self, ts: i64, after: f64, thresholds: &[Option<u32>]) {
        self.end = after;
        let delta = after - self.start;
        for ((crossed_at, change), &threshold) in self
            .crossed_at
            .iter_mut()
            .zip([delta, -delta])
            .zip(thresholds)
        {
            if threshold.is_some_and(|minimum| change >= f64::from(minimum)) {
                crossed_at.get_or_insert(ts);
            } else {
                *crossed_at = None;
            }
        }
    }
}

fn flow_buckets(
    changes: &[DepthChange],
    start: i64,
    query: &FlowQuery,
    match_trades: bool,
    trades: [Vec<(i64, f64)>; 2],
    mut collect: impl FnMut(FlowBucket),
) {
    let warmup = if match_trades {
        start.saturating_sub(TRADE_MATCH_NS)
    } else {
        start
    };
    let first = changes.partition_point(|change| change.0 < warmup);
    let end = changes.partition_point(|change| change.0 < query.read_until);
    if first >= end {
        return;
    }
    let mut trades = trades.map(|trades| TradeMatch { trades, cursor: 0 });
    let mut before = first.checked_sub(1).map_or([0.0; 2], |i| changes[i].1);
    let mut pending: Option<FlowBucket> = None;
    for (index, &(ts, after)) in changes.iter().enumerate().take(end).skip(first) {
        let prior = before;
        let delta: [f64; 2] = std::array::from_fn(|side| after[side] - before[side]);
        before = after;
        if index == 0 && query.initial_depth_ns.is_some_and(|initial| ts <= initial) {
            continue;
        }
        let decreases = delta.map(|size| (-size).max(0.0));
        let cancelled = if match_trades {
            std::array::from_fn(|side| trades[side].unmatched(ts, decreases[side]))
        } else {
            [0.0; 2]
        };
        if ts < start {
            continue;
        }
        let volumes = [
            query.metric.size(delta.map(|size| size.max(0.0))),
            query.metric.size(decreases),
            query.metric.size(cancelled),
        ];
        if volumes == [0.0; 3] {
            continue;
        }
        let from = window_start(ts);
        if pending.as_ref().is_some_and(|bucket| bucket.from != from) {
            collect(pending.take().unwrap());
        }
        let bucket = pending.get_or_insert_with(|| FlowBucket {
            from,
            ..FlowBucket::default()
        });
        let resting_thresholds = &query.markers[..2];
        if resting_thresholds.iter().any(Option::is_some) {
            let start = query.metric.size(prior);
            bucket
                .resting_change
                .get_or_insert(RestingChange {
                    start,
                    end: start,
                    crossed_at: [None; 2],
                })
                .update(ts, query.metric.size(after), resting_thresholds);
        }
        for (total, volume) in bucket.volumes.iter_mut().zip(volumes) {
            *total += volume;
        }
        if volumes[2] > 0.0 {
            bucket.last_cancel = ts;
        }
    }
    if let Some(bucket) = pending {
        collect(bucket);
    }
}

fn rasterize_net_bucket(row: &mut [f32], columns: &[(usize, i64, i64)], bucket: &FlowBucket) {
    let net = bucket.volumes[0] - bucket.volumes[2];
    if net == 0.0 {
        return;
    }
    let end = bucket.from.saturating_add(FLOW_WINDOW_NS);
    let first = columns.partition_point(|&(_, _, until)| until <= bucket.from);
    for &(col, from, until) in columns[first..]
        .iter()
        .take_while(|&&(_, from, _)| from < end)
    {
        let overlap = until.min(end) - from.max(bucket.from);
        row[col] += (net * overlap as f64 / (until - from) as f64) as f32;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Study {
    #[default]
    Resting,
    Added,
    Decreased,
    Cancelled,
    Net,
}

impl Study {
    pub(crate) const COUNT: usize = 5;
    pub(crate) const ALL: [Self; 5] = [
        Self::Resting,
        Self::Added,
        Self::Decreased,
        Self::Cancelled,
        Self::Net,
    ];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Resting => "Resting liquidity",
            Self::Added => "Liquidity added",
            Self::Decreased => "Depth decreases",
            Self::Cancelled => "Estimated cancellations",
            Self::Net => "Net liquidity added",
        }
    }

    pub(crate) fn requires_trades(self) -> bool {
        matches!(self, Self::Cancelled | Self::Net)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Studies {
    pub study: Study,
    pub filter_levels: bool,
    pub min_size: u32,
    pub hold_ms: u32,
}

impl Default for Studies {
    fn default() -> Self {
        Self {
            study: Study::Resting,
            filter_levels: false,
            min_size: 0,
            hold_ms: 250,
        }
    }
}

impl Studies {
    pub(crate) fn active(self) -> bool {
        self.study != Study::Resting
            || (self.filter_levels && (self.min_size > 0 || self.hold_ms > 0))
    }

    pub(crate) fn lookback_ns(self) -> i64 {
        match self.study {
            Study::Resting => i64::from(self.hold_ms) * 1_000_000,
            _ => FLOW_LOOKBACK_NS,
        }
    }
}

fn visible_columns(times: &[Option<(i64, i64)>]) -> Vec<(usize, i64, i64)> {
    times
        .iter()
        .enumerate()
        .filter_map(|(col, interval)| interval.map(|(from, until)| (col, from, until)))
        .collect()
}

pub(crate) fn rasterize_resting(
    spec: &RasterSpec,
    times: &[Option<(i64, i64)>],
    depth: &DepthHistory,
    metric: HeatMetric,
    settings: Studies,
) -> Vec<f32> {
    let mut cells = vec![0.0; spec.rows * spec.cols];
    let columns = visible_columns(times);
    let (Some(&(_, from, _)), Some(&(_, _, until))) = (columns.first(), columns.last()) else {
        return cells;
    };
    let (top, bottom) = spec.price_range();
    for (price, level) in depth.levels_between(bottom, top) {
        let row = spec.k_hi - (price / spec.row_p).round() as i64;
        if !(0..spec.rows as i64).contains(&row) {
            continue;
        }
        let cells = &mut cells[row as usize * spec.cols..][..spec.cols];
        let changes = level.changes();
        let end = changes.partition_point(|change| change.0 < until);
        let start = changes[..end]
            .partition_point(|change| change.0 < from.saturating_sub(settings.lookback_ns()))
            .saturating_sub(1);
        resting(cells, &columns, &changes[start..end], metric, settings);
    }
    cells
}

const NO_LIQUIDITY: i64 = i64::MAX;

fn resting(
    cells: &mut [f32],
    columns: &[(usize, i64, i64)],
    changes: &[DepthChange],
    metric: HeatMetric,
    settings: Studies,
) {
    let min_size = f64::from(settings.min_size);
    let hold = settings.lookback_ns();
    let mut visible_at = [NO_LIQUIDITY; 2];
    let mut sizes = [0.0; 2];
    let mut next = changes.iter().peekable();
    for &(col, from, until) in columns {
        let mut start = from;
        let mut area = [0.0; 2];
        while let Some(&&(ts, after)) = next.peek().filter(|point| point.0 < until) {
            add_area(&mut area, sizes, visible_at, start, ts);
            start = start.max(ts);
            for (side, &size) in after.iter().enumerate() {
                if size <= 0.0 || size < min_size {
                    visible_at[side] = NO_LIQUIDITY;
                } else if visible_at[side] == NO_LIQUIDITY {
                    visible_at[side] = ts.saturating_add(hold);
                }
            }
            sizes = after;
            next.next();
        }
        add_area(&mut area, sizes, visible_at, start, until);
        let size = metric.size(area) / (until - from) as f64;
        cells[col] = cells[col].max(size as f32);
    }
}

fn add_area(area: &mut [f64; 2], sizes: [f64; 2], visible_at: [i64; 2], from: i64, until: i64) {
    for side in 0..2 {
        area[side] += sizes[side] * until.saturating_sub(from.max(visible_at[side])).max(0) as f64;
    }
}

struct TradeMatch {
    trades: Vec<(i64, f64)>,
    cursor: usize,
}

impl TradeMatch {
    fn unmatched(&mut self, ts: i64, mut decrease: f64) -> f64 {
        while decrease > 0.0 {
            let Some((trade_ts, remaining)) = self.trades.get_mut(self.cursor) else {
                break;
            };
            if *trade_ts > ts {
                break;
            }
            if *trade_ts >= ts.saturating_sub(TRADE_MATCH_NS) {
                let used = decrease.min(*remaining);
                decrease -= used;
                *remaining -= used;
                if *remaining > 0.0 {
                    break;
                }
            }
            self.cursor += 1;
        }
        decrease
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::{prepare_market_history, DataSource, OrderFlowEvent};

    struct Source(Vec<OrderFlowEvent>);
    impl DataSource for Source {
        fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
            Box::new(self.0.into_iter())
        }
    }

    fn query(from: i64, until: i64) -> FlowQuery {
        FlowQuery {
            prices: (99.0, 101.0),
            marker_prices: (99.0, 101.0),
            visible_times: (from, until),
            read_until: until,
            initial_depth_ns: Some(0),
            metric: HeatMetric::Bid,
            tick: 0.25,
            markers: [Some(40), Some(40), Some(40)],
            marker_limit: 5,
        }
    }

    #[test]
    fn flow_markers_use_fixed_windows_ignore_snapshots_and_respect_the_replay_cutoff() {
        let level = |second, price, size| OrderFlowEvent::SetLevel {
            ts_ns: second * SEC_NS,
            price,
            size,
            side: Side::Buy,
        };
        let (_, depth) = prepare_market_history(Box::new(Source(vec![
            level(0, 100.0, 100.0),
            level(1, 100.0, 124.0),
            level(2, 100.0, 80.0),
            level(3, 100.0, 99.0),
            level(4, 100.25, 12.0),
            level(5, 100.0, 118.0),
            level(6, 100.0, 154.0),
            level(7, 100.0, 155.0),
        ])));
        let run = |from, until, limit| {
            let query = FlowQuery {
                marker_limit: limit,
                ..query(from, until)
            };
            flow_layers(
                &depth,
                &query,
                None,
                [(2 * SEC_NS, 100.0, 5.0, Side::Sell)].into_iter(),
            )
            .markers
        };
        assert!(run(2 * SEC_NS, 6 * SEC_NS, 5).is_empty());
        assert_eq!(
            run(0, 10 * SEC_NS, 1)[0],
            FlowMarker {
                study: Study::Added,
                ts: 6 * SEC_NS,
                price: 100.0,
                volume: 56.0,
                resting: Some([99.0, 155.0]),
            }
        );
        assert!(run(0, 2 * SEC_NS, 5).is_empty());
        assert!(run(5 * SEC_NS, 9 * SEC_NS, 5).is_empty());
    }

    #[test]
    fn depth_decrease_is_anchored_to_the_drop_that_holds_through_the_window() {
        let level = |second, size| OrderFlowEvent::SetLevel {
            ts_ns: second * SEC_NS,
            price: 100.0,
            size,
            side: Side::Buy,
        };
        let (_, depth) = prepare_market_history(Box::new(Source(vec![
            level(0, 100.0),
            level(1, 40.0),
            level(2, 80.0),
            level(3, 42.0),
            level(4, 43.0),
        ])));
        let query = FlowQuery {
            markers: [None, Some(40), None],
            ..query(0, 5 * SEC_NS)
        };
        assert_eq!(
            flow_layers(&depth, &query, None, std::iter::empty()).markers,
            [FlowMarker {
                study: Study::Decreased,
                ts: 3 * SEC_NS,
                price: 100.0,
                volume: 57.0,
                resting: Some([100.0, 43.0]),
            },]
        );
    }

    #[test]
    fn flow_uses_trade_volume_once_and_never_uses_a_future_trade() {
        let boundary = 5 * SEC_NS;
        let changes = [
            (0, [15.0, 0.0]),
            (boundary - 2_000_000, [13.0, 0.0]), // warmup consumes 2
            (boundary + 1_000_000, [10.0, 0.0]), // 2 traded, 1 estimated cancelled
            (boundary + 3_000_000, [6.0, 2.0]),  // 4 cancelled on bid, 2 added on ask
            (boundary + 6_000_000, [3.0, 2.0]),  // 3 traded
        ];
        let query = FlowQuery {
            metric: HeatMetric::Total,
            ..query(boundary, 10 * SEC_NS)
        };
        let prints = [(boundary - 3_000_000, 4.0), (boundary + 4_000_000, 3.0)];
        let mut buckets = Vec::new();
        flow_buckets(
            &changes,
            boundary,
            &query,
            true,
            [prints.to_vec(), vec![]],
            |bucket| buckets.push(bucket),
        );
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].volumes, [2.0, 10.0, 5.0]);
    }

    #[test]
    fn persistence_is_causal_and_restarts_when_depth_drops_below_the_minimum() {
        let ms = 1_000_000;
        let columns = [
            (0, 0, 10 * ms),
            (1, 10 * ms, 20 * ms),
            (2, 20 * ms, 30 * ms),
        ];
        let changes = [
            (0, [12.0, 10.0]),
            (12 * ms, [8.0, 10.0]),
            (16 * ms, [12.0, 10.0]),
        ];
        let settings = Studies {
            filter_levels: true,
            min_size: 10,
            hold_ms: 5,
            ..Studies::default()
        };
        let mut bid = [0.0; 3];
        resting(&mut bid, &columns, &changes, HeatMetric::Bid, settings);
        assert_eq!(bid, [6.0, 2.4, 10.8]);
        let mut ask = [0.0; 3];
        resting(&mut ask, &columns, &changes, HeatMetric::Ask, settings);
        assert_eq!(ask, [5.0, 10.0, 10.0]);
    }

    #[test]
    fn resting_filter_uses_depth_before_the_visible_range_for_long_holds() {
        let (_, depth) = prepare_market_history(Box::new(Source(vec![OrderFlowEvent::SetLevel {
            ts_ns: 0,
            price: 100.0,
            size: 12.0,
            side: Side::Buy,
        }])));
        let spec = RasterSpec {
            c_lo: 0,
            cols: 2,
            col_w: 1.0,
            k_hi: 400,
            rows: 1,
            row_p: 0.25,
        };
        for seconds in [60, 1800] {
            let threshold = i64::from(seconds) * SEC_NS;
            let times = [
                Some((threshold - SEC_NS, threshold)),
                Some((threshold, threshold + SEC_NS)),
            ];
            let settings = Studies {
                filter_levels: true,
                min_size: 10,
                hold_ms: seconds * 1_000,
                ..Studies::default()
            };
            assert_eq!(
                rasterize_resting(&spec, &times, &depth, HeatMetric::Bid, settings),
                [0.0, 12.0]
            );
        }
    }

    #[test]
    fn net_keeps_fixed_bucket_values_across_zoom_and_excludes_future_replay_changes() {
        let level = |ts_ns, size| OrderFlowEvent::SetLevel {
            ts_ns,
            size,
            price: 100.0,
            side: Side::Buy,
        };
        let (_, depth) = prepare_market_history(Box::new(Source(vec![
            level(0, 15.0),
            level(SEC_NS, 7.0),
            level(3 * SEC_NS, 1.0),
            level(6 * SEC_NS, 7.0),
        ])));
        let prints = [
            (SEC_NS, 100.0, 2.0, Side::Sell),
            (SEC_NS, 100.0, 8.0, Side::Buy),
            (SEC_NS, 100.25, 8.0, Side::Sell),
        ];
        let run = |until, times: &[Option<(i64, i64)>]| {
            let spec = RasterSpec {
                c_lo: 0,
                cols: times.len(),
                col_w: 1.0,
                k_hi: 400,
                rows: 1,
                row_p: 0.25,
            };
            let query = query(0, until);
            flow_layers(&depth, &query, Some((&spec, times)), prints.into_iter()).net
        };
        assert_eq!(
            run(
                2 * SEC_NS,
                &[Some((0, SEC_NS)), Some((SEC_NS, 2 * SEC_NS)), None]
            ),
            [-6.0, -6.0, 0.0]
        );
        assert_eq!(
            run(
                10 * SEC_NS,
                &[Some((0, 5 * SEC_NS)), Some((5 * SEC_NS, 10 * SEC_NS))]
            ),
            [-12.0, 6.0]
        );
        assert_eq!(run(10 * SEC_NS, &[Some((0, 10 * SEC_NS))]), [-3.0]);
    }
}
