//! Shared causal order-flow features for ES footprint strategies, plus the single
//! [`FeatureExtractor`] used for ML dataset export (spec: features are computed once, consumed
//! by both rule strategies and training data).

pub mod dataset;
pub mod features;

pub use dataset::{
    build_dataset, continuation_label, export_csv, failure_label, ContinuationLabelConfig,
    DatasetConfig, DatasetKind, FailureLabelConfig, FeatureExtractor, MlDatasetRow,
};
pub use features::TradeFlowFeatures;

use accelerando_core::Registry;

/// Register the trade-flow feature indicator into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_indicator::<TradeFlowFeatures>("trade_flow_features");
}

#[cfg(test)]
mod tests {
    use super::features as tf;
    use super::*;
    use accelerando_core::market_time::{days_from_civil, NS_PER_SEC};
    use accelerando_core::{Configurable, Footprint, Indicator, Level, OrderFlowEvent, Params, Side};

    const TICK: f64 = 0.25;

    /// ts for an ET (winter, EST=UTC-5) wall-clock time on a civil date.
    fn et_ts(year: i32, month: i32, day: i32, hour: i64, minute: i64) -> i64 {
        (days_from_civil(year, month, day) * 86_400 + (hour + 5) * 3600 + minute * 60) * NS_PER_SEC
    }

    fn bar(ts_ns: i64, open: f64, high: f64, low: f64, close: f64, buy: f64, sell: f64) -> Footprint {
        let mid = ((high + low) / 2.0 / TICK).round() * TICK;
        Footprint {
            ts_first_ns: ts_ns,
            ts_last_ns: ts_ns + 59 * NS_PER_SEC,
            open,
            high,
            low,
            close,
            volume: buy + sell,
            trades: ((buy + sell).max(1.0)) as u32,
            delta: buy - sell,
            poc: mid,
            ladder: vec![Level {
                price: mid,
                buy_vol: buy,
                sell_vol: sell,
            }],
            values: Default::default(),
            tags: Default::default(),
            plots: Vec::new(),
        }
    }

    fn indicator() -> TradeFlowFeatures {
        TradeFlowFeatures::build(&Params::default())
    }

    fn run(ind: &mut TradeFlowFeatures, bars: &mut [Footprint]) {
        let mut history: Vec<Footprint> = Vec::new();
        for fp in bars.iter_mut() {
            ind.on_footprint(fp, &history);
            history.push(fp.clone());
        }
    }

    #[test]
    fn delta_and_delta_ratio_are_correct() {
        let mut fp = bar(et_ts(2026, 1, 5, 10, 0), 100.0, 100.5, 99.75, 100.25, 30.0, 10.0);
        indicator().on_footprint(&mut fp, &[]);
        assert_eq!(fp.delta, 20.0);
        assert_eq!(fp.values[tf::BUY_VOLUME], 30.0);
        assert_eq!(fp.values[tf::SELL_VOLUME], 10.0);
        assert!((fp.values[tf::DELTA_RATIO] - 0.5).abs() < 1e-12);
        assert!((fp.values[tf::BAR_MOVE_TICKS] - 1.0).abs() < 1e-12);
        assert!((fp.values[tf::BAR_RANGE_TICKS] - 3.0).abs() < 1e-12);
        // close_location = (100.25 - 99.75) / 0.75
        assert!((fp.values[tf::CLOSE_LOCATION] - 2.0 / 3.0).abs() < 1e-12);
        // delta_efficiency = 1 tick / 20 delta
        assert!((fp.values[tf::DELTA_EFFICIENCY] - 0.05).abs() < 1e-12);
    }

    #[test]
    fn cvd_resets_per_eastern_day() {
        let mut ind = indicator();
        let mut day1a = bar(et_ts(2026, 1, 5, 10, 0), 100.0, 100.5, 100.0, 100.5, 30.0, 10.0);
        let mut day1b = bar(et_ts(2026, 1, 5, 10, 1), 100.5, 101.0, 100.5, 101.0, 25.0, 5.0);
        let mut day2 = bar(et_ts(2026, 1, 6, 9, 31), 101.0, 101.5, 101.0, 101.5, 12.0, 2.0);
        ind.on_footprint(&mut day1a, &[]);
        ind.on_footprint(&mut day1b, &[]);
        ind.on_footprint(&mut day2, &[]);
        assert_eq!(day1a.values[tf::CVD], 20.0);
        assert_eq!(day1b.values[tf::CVD], 40.0);
        assert_eq!(day1b.values[tf::CVD_CHANGE_1], 20.0);
        // New ET day: CVD restarts from this bar's delta and change_1 has no same-day history.
        assert_eq!(day2.values[tf::CVD], 10.0);
        assert!(!day2.values.contains_key(tf::CVD_CHANGE_1));
    }

    #[test]
    fn zscore_uses_only_past_data() {
        let mut ind = indicator();
        let base_ts = et_ts(2026, 1, 5, 10, 0);
        // 40 calm bars with alternating small deltas, then one huge-delta bar.
        let mut history: Vec<Footprint> = Vec::new();
        for i in 0..40 {
            let (b, s) = if i % 2 == 0 { (12.0, 8.0) } else { (8.0, 12.0) };
            let mut fp = bar(base_ts + i * 60 * NS_PER_SEC, 100.0, 100.25, 99.75, 100.0, b, s);
            ind.on_footprint(&mut fp, &history);
            history.push(fp);
        }
        // z of the calm bars stays small…
        assert!(history[39].values[tf::DELTA_Z].abs() < 2.0);
        // …and the outlier is scored against calm history only, so it is extreme.
        let mut spike = bar(base_ts + 40 * 60 * NS_PER_SEC, 100.0, 101.0, 100.0, 101.0, 500.0, 20.0);
        ind.on_footprint(&mut spike, &history);
        assert!(spike.values[tf::DELTA_Z] > 5.0);
    }

    #[test]
    fn features_do_not_change_when_future_bars_change() {
        let base_ts = et_ts(2026, 1, 5, 10, 0);
        let build = |tail_buy: f64| {
            let mut bars: Vec<Footprint> = (0..50)
                .map(|i| {
                    let b = 10.0 + (i % 5) as f64;
                    bar(base_ts + i * 60 * NS_PER_SEC, 100.0 + i as f64 * 0.25, 100.5 + i as f64 * 0.25, 99.75 + i as f64 * 0.25, 100.25 + i as f64 * 0.25, b, 8.0)
                })
                .collect();
            // Mutate only the future (last 10 bars).
            for fp in bars.iter_mut().skip(40) {
                fp.delta = tail_buy - 8.0;
                fp.volume = tail_buy + 8.0;
                fp.high += 5.0;
                fp.close += 4.0;
            }
            let mut ind = indicator();
            run(&mut ind, &mut bars);
            bars
        };
        let a = build(10.0);
        let b = build(900.0);
        for i in 0..40 {
            assert_eq!(a[i].values, b[i].values, "bar {i} features changed with future data");
        }
    }

    #[test]
    fn session_vwap_is_volume_weighted_ladder_price() {
        let mut ind = indicator();
        let ts = et_ts(2026, 1, 5, 10, 0);
        let mut fp1 = bar(ts, 100.0, 100.0, 100.0, 100.0, 10.0, 10.0); // 20 vol @ 100
        ind.on_footprint(&mut fp1, &[]);
        assert!((fp1.values[tf::SESSION_VWAP] - 100.0).abs() < 1e-9);
        let mut fp2 = bar(ts + 60 * NS_PER_SEC, 102.0, 102.0, 102.0, 102.0, 30.0, 30.0); // 60 vol @ 102
        ind.on_footprint(&mut fp2, &[fp1.clone()]);
        // vwap = (20·100 + 60·102) / 80 = 101.5 ; distance = (102 - 101.5)/0.25 = 2 ticks
        assert!((fp2.values[tf::SESSION_VWAP] - 101.5).abs() < 1e-9);
        assert!((fp2.values[tf::DIST_VWAP_TICKS] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn recent_range_excludes_current_bar() {
        let mut ind = indicator();
        let ts = et_ts(2026, 1, 5, 10, 0);
        let mut prev = bar(ts, 100.0, 101.0, 99.0, 100.0, 10.0, 10.0);
        ind.on_footprint(&mut prev, &[]);
        // Current bar spikes far above the prior range; position is measured against the
        // *previous* bars' [99, 101] range only.
        let mut cur = bar(ts + 60 * NS_PER_SEC, 100.0, 105.0, 100.0, 105.0, 10.0, 10.0);
        ind.on_footprint(&mut cur, &[prev.clone()]);
        assert!((cur.values[tf::DIST_RECENT_HIGH_TICKS] - (101.0 - 105.0) / TICK).abs() < 1e-9);
        assert!(cur.values[tf::RECENT_RANGE_POS] > 1.0);
        // First bar has no prior window at all.
        assert!(!prev.values.contains_key(tf::RECENT_RANGE_POS));
    }

    #[test]
    fn session_extremes_come_from_prior_bars_of_the_day() {
        let mut ind = indicator();
        let ts = et_ts(2026, 1, 5, 10, 0);
        let mut fp1 = bar(ts, 100.0, 101.0, 99.0, 100.0, 10.0, 10.0);
        ind.on_footprint(&mut fp1, &[]);
        assert!(!fp1.values.contains_key(tf::DIST_SESSION_HIGH_TICKS));
        let mut fp2 = bar(ts + 60 * NS_PER_SEC, 100.0, 100.5, 99.5, 100.5, 10.0, 10.0);
        ind.on_footprint(&mut fp2, &[fp1.clone()]);
        assert!((fp2.values[tf::DIST_SESSION_HIGH_TICKS] - (101.0 - 100.5) / TICK).abs() < 1e-9);
        assert!((fp2.values[tf::DIST_SESSION_LOW_TICKS] - (100.5 - 99.0) / TICK).abs() < 1e-9);
    }

    #[test]
    fn diagonal_imbalance_direction_is_correct() {
        let mut fp = Footprint {
            ladder: vec![
                Level { price: 100.00, buy_vol: 5.0, sell_vol: 20.0 },
                Level { price: 100.25, buy_vol: 90.0, sell_vol: 25.0 }, // buy: 90 ≥ 3×20
                Level { price: 100.50, buy_vol: 80.0, sell_vol: 20.0 }, // buy: 80 ≥ 3×25
                Level { price: 100.75, buy_vol: 70.0, sell_vol: 5.0 },  // buy: 70 ≥ 3×20
            ],
            ..bar(et_ts(2026, 1, 5, 10, 0), 100.0, 100.75, 100.0, 100.75, 245.0, 70.0)
        };
        indicator().on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[tf::POS_IMBALANCE_COUNT], 3.0);
        assert_eq!(fp.values[tf::POS_STACK_MAX_LAYERS], 3.0);
        assert_eq!(fp.values[tf::NEG_IMBALANCE_COUNT], 0.0);
        assert!(fp.values[tf::MAX_POS_IMBALANCE_RATIO] >= 3.0);
    }

    #[test]
    fn empty_ladder_and_zero_volume_do_not_panic() {
        let mut ind = indicator();
        let mut fp = Footprint::seed(et_ts(2026, 1, 5, 10, 0), 100.0);
        ind.on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[tf::DELTA_RATIO], 0.0);
        assert_eq!(fp.values[tf::POS_IMBALANCE_COUNT], 0.0);
    }

    #[test]
    fn large_trades_use_past_only_threshold() {
        let mut p = Params::default();
        p.set(
            "large_trade_mode",
            accelerando_core::ParamValue::Str("percentile".into()),
        );
        let mut ind = TradeFlowFeatures::build(&p);
        let ts = et_ts(2026, 1, 5, 10, 0);
        // Feed 200 small trades (size 2) into bar 1.
        for i in 0..200 {
            ind.on_event(&OrderFlowEvent::Trade {
                ts_ns: ts + i,
                price: 100.0,
                size: 2.0,
                aggressor: Side::Buy,
            });
        }
        let mut fp1 = bar(ts, 100.0, 100.25, 100.0, 100.25, 200.0, 200.0);
        ind.on_footprint(&mut fp1, &[]);
        // First bar has no prior distribution → fixed floor (10) applies; size-2 trades are small.
        assert_eq!(fp1.values[tf::LARGE_TRADE_VOLUME], 0.0);
        // Bar 2 carries one size-50 trade; the p95 threshold from bar 1's sizes is ~2 → floored 10.
        let ts2 = ts + 60 * NS_PER_SEC;
        ind.on_event(&OrderFlowEvent::Trade {
            ts_ns: ts2,
            price: 100.25,
            size: 50.0,
            aggressor: Side::Buy,
        });
        let mut fp2 = bar(ts2, 100.25, 100.5, 100.25, 100.5, 30.0, 20.0);
        ind.on_footprint(&mut fp2, &[fp1.clone()]);
        assert_eq!(fp2.values[tf::LARGE_TRADE_VOLUME], 50.0);
    }

    // ---- labels ---------------------------------------------------------------------------------

    fn plain(open: f64, high: f64, low: f64, close: f64, i: i64) -> Footprint {
        bar(et_ts(2026, 1, 5, 10, 0) + i * 60 * NS_PER_SEC, open, high, low, close, 10.0, 10.0)
    }

    #[test]
    fn continuation_label_target_stop_and_conservative_tie() {
        let cfg = ContinuationLabelConfig::default(); // target 8t = 2.0, stop 5t = 1.25
        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 102.5, 100.0, 102.0, 1), // hits 102.0 target
        ];
        assert_eq!(continuation_label(&fps, 0, 1, TICK, &cfg), Some(1.0));

        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 100.5, 98.0, 98.5, 1), // hits 98.75 stop
        ];
        assert_eq!(continuation_label(&fps, 0, 1, TICK, &cfg), Some(0.0));

        // One bar touching both stop and target resolves as stop (conservative).
        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 103.0, 98.0, 100.0, 1),
        ];
        assert_eq!(continuation_label(&fps, 0, 1, TICK, &cfg), Some(0.0));

        // Nothing touched inside the horizon → unlabeled.
        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 100.5, 99.5, 100.0, 1),
        ];
        assert_eq!(continuation_label(&fps, 0, 1, TICK, &cfg), None);
    }

    #[test]
    fn failure_label_requires_low_mfe_then_reverse_break() {
        let cfg = FailureLabelConfig::default(); // 8t = 2.0 both ways, mfe ≤ 0.2·ATR
        let atr = 2.0; // mfe allowance = 0.4
        // Buy candidate at 100: stalls (mfe 0.25 ≤ 0.4), then breaks down 2.0 → label 1.
        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 100.25, 99.5, 99.75, 1),
            plain(99.75, 99.75, 97.5, 97.75, 2),
        ];
        assert_eq!(failure_label(&fps, 0, 1, TICK, atr, &cfg), Some(1.0));

        // Continuation hit first → 0.
        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 102.5, 100.0, 102.0, 1),
        ];
        assert_eq!(failure_label(&fps, 0, 1, TICK, atr, &cfg), Some(0.0));

        // Reverse hit but the response MFE was big (aggressors did move price) → unlabeled.
        let fps = vec![
            plain(100.0, 100.0, 100.0, 100.0, 0),
            plain(100.0, 101.0, 100.0, 100.5, 1), // mfe 1.0 > 0.4
            plain(100.5, 100.5, 97.5, 97.75, 2),
        ];
        assert_eq!(failure_label(&fps, 0, 1, TICK, atr, &cfg), None);
    }

    #[test]
    fn export_csv_writes_candidate_rows() {
        // Build 45 bars so z-scores exist, ending with a strong-delta bar.
        let mut ind = indicator();
        let base_ts = et_ts(2026, 1, 5, 10, 0);
        let mut bars: Vec<Footprint> = (0..45)
            .map(|i| {
                let (b, s) = if i % 2 == 0 { (12.0, 8.0) } else { (8.0, 12.0) };
                bar(base_ts + i * 60 * NS_PER_SEC, 100.0, 100.25, 99.75, 100.0, b, s)
            })
            .collect();
        bars.push(bar(base_ts + 45 * 60 * NS_PER_SEC, 100.0, 100.25, 99.75, 100.0, 400.0, 10.0));
        // A follow-up bar so the failure label can resolve.
        bars.push(bar(base_ts + 46 * 60 * NS_PER_SEC, 100.0, 100.0, 97.0, 97.25, 10.0, 10.0));
        run(&mut ind, &mut bars);

        let cfg = DatasetConfig {
            drop_unlabeled: false,
            ..DatasetConfig::default()
        };
        let mut buf: Vec<u8> = Vec::new();
        let n = export_csv(&mut buf, &bars, TICK, DatasetKind::FailureReversal, &cfg).unwrap();
        assert!(n >= 1);
        let text = String::from_utf8(buf).unwrap();
        let header = text.lines().next().unwrap();
        assert!(header.starts_with("timestamp_ns,session_date,symbol,contract,bar_index"));
        assert!(header.contains("delta_zscore"));
        assert!(header.ends_with("failure_reversal_label"));
        assert_eq!(text.lines().count(), n + 1);
    }
}
