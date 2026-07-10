use accelerando_core::{Configurable, Footprint, Indicator, ParamSpec, Params, Plot};
use std::collections::VecDeque;

pub const BUY_STACK: &str = "stacked_imbalance_buy";
pub const BUY_STACK_LOW: &str = "stacked_imbalance_buy_low";
pub const BUY_STACK_HIGH: &str = "stacked_imbalance_buy_high";
pub const BUY_STACK_SIZE: &str = "stacked_imbalance_buy_size";
pub const BUY_STACK_VOL: &str = "stacked_imbalance_buy_vol";
pub const BUY_STACK_RATIO_MAX: &str = "stacked_imbalance_buy_ratio_max";
pub const SELL_STACK: &str = "stacked_imbalance_sell";
pub const SELL_STACK_LOW: &str = "stacked_imbalance_sell_low";
pub const SELL_STACK_HIGH: &str = "stacked_imbalance_sell_high";
pub const SELL_STACK_SIZE: &str = "stacked_imbalance_sell_size";
pub const SELL_STACK_VOL: &str = "stacked_imbalance_sell_vol";
pub const SELL_STACK_RATIO_MAX: &str = "stacked_imbalance_sell_ratio_max";
pub const VOLUME_THRESHOLD: &str = "stacked_imbalance_volume_threshold";

const BUY_COLOR: &str = "#6d6dff";
const SELL_COLOR: &str = "#ff6d6d";
/// Histogram bucket cap for the rolling volume-percentile filter (volumes above clamp here).
const HIST_BUCKETS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VolumeMode {
    /// Minimum aggressive volume is the fixed `min_volume` parameter.
    Fixed,
    /// Minimum aggressive volume is a rolling percentile of recent per-level aggressive
    /// volumes (floored at `min_volume`), so the filter adapts across instruments.
    Percentile,
}

/// One detected stack of same-direction diagonal imbalances inside a single bar.
#[derive(Clone, Copy, Debug)]
struct Stack {
    low: f64,
    high: f64,
    levels: usize,
    aggr_vol: f64,
    ratio_max: f64,
}

/// ATAS-style diagonal (bid×ask) imbalance detector.
///
/// For each price level `p` in a bar's ladder:
/// - buy imbalance:  `ask(p) >= ratio × bid(p - tick)` with `bid(p - tick) > 0`
/// - sell imbalance: `bid(p) >= ratio × ask(p + tick)` with `ask(p + tick) > 0`
///
/// plus an absolute aggressive-volume filter (fixed, or a rolling percentile of recent
/// per-level aggressive volumes). Runs of `min_stack`+ adjacent same-direction imbalances
/// merge into one stacked-imbalance zone, published as values and as `PriceBox` plots
/// (group `imbalance`) that the web studio extends right until price closes through them.
pub struct StackedImbalance {
    ratio: f64,
    min_stack: usize,
    tick: f64,
    volume_mode: VolumeMode,
    min_volume: f64,
    percentile: f64,
    lookback_bars: usize,
    plot_zones: bool,
    max_zones_per_side: usize,
    // Rolling histogram of per-level aggressive volumes for the percentile filter.
    hist: Vec<u32>,
    hist_total: u64,
    window: VecDeque<Vec<u16>>,
    seq: u64,
}

impl Configurable for StackedImbalance {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .float("ratio", 3.0, 1.1, 20.0)
            .int("min_stack", 3, 1, 12, 1)
            .fixed_float("tick", 0.25)
            .choice("volume_mode", "percentile", &["percentile", "fixed"])
            .float("min_volume", 10.0, 0.0, 5000.0)
            .float("percentile", 0.70, 0.0, 0.999)
            .int("lookback_bars", 500, 10, 20000, 10)
            .int("plot_zones", 1, 0, 1, 1)
            .int("max_zones_per_side", 2, 1, 8, 1)
    }

    fn from_params(p: &Params) -> Self {
        Self {
            ratio: p.float("ratio", 3.0).max(1.0),
            min_stack: p.usize("min_stack", 3).max(1),
            tick: p.float("tick", 0.25).max(f64::EPSILON),
            volume_mode: match p.str("volume_mode", "percentile").as_str() {
                "fixed" => VolumeMode::Fixed,
                _ => VolumeMode::Percentile,
            },
            min_volume: p.float("min_volume", 10.0).max(0.0),
            percentile: p.float("percentile", 0.70).clamp(0.0, 0.999),
            lookback_bars: p.usize("lookback_bars", 500).max(1),
            plot_zones: p.int("plot_zones", 1) != 0,
            max_zones_per_side: p.usize("max_zones_per_side", 2).max(1),
            hist: vec![0; HIST_BUCKETS],
            hist_total: 0,
            window: VecDeque::new(),
            seq: 0,
        }
    }
}

impl Indicator for StackedImbalance {
    fn name(&self) -> &str {
        "stacked_imbalance"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        let vol_threshold = self.volume_threshold();
        fp.values
            .insert(VOLUME_THRESHOLD.to_string(), vol_threshold);

        let (buy_stacks, sell_stacks) = self.find_stacks(fp, vol_threshold);
        self.publish(fp, &buy_stacks, true);
        self.publish(fp, &sell_stacks, false);
        if self.plot_zones {
            self.plot(fp, &buy_stacks, true);
            self.plot(fp, &sell_stacks, false);
        }

        // Feed this bar's per-level aggressive volumes into the rolling window last, so the
        // threshold applied above only ever saw prior bars.
        self.push_window(fp);
    }
}

impl StackedImbalance {
    fn volume_threshold(&self) -> f64 {
        if self.volume_mode == VolumeMode::Fixed || self.hist_total == 0 {
            return self.min_volume;
        }
        let rank = (self.hist_total as f64 * self.percentile).floor() as u64;
        let mut seen = 0u64;
        for (bucket, &count) in self.hist.iter().enumerate() {
            seen += u64::from(count);
            if seen > rank {
                return (bucket as f64).max(self.min_volume);
            }
        }
        self.min_volume
    }

    fn find_stacks(&self, fp: &Footprint, vol_threshold: f64) -> (Vec<Stack>, Vec<Stack>) {
        // Ladder is ascending by price; key levels by tick index so gaps break adjacency.
        let keyed: Vec<(i64, f64, f64)> = fp
            .ladder
            .iter()
            .map(|lv| ((lv.price / self.tick).round() as i64, lv.buy_vol, lv.sell_vol))
            .collect();
        let mut buy_hits: Vec<(i64, f64, f64)> = Vec::new(); // (key, aggr_vol, ratio)
        let mut sell_hits: Vec<(i64, f64, f64)> = Vec::new();
        for (i, &(key, buy, sell)) in keyed.iter().enumerate() {
            // Diagonal below: bid volume one tick under this level's ask volume.
            if i > 0 && keyed[i - 1].0 == key - 1 {
                let bid_below = keyed[i - 1].2;
                if bid_below > 0.0 && buy >= self.ratio * bid_below && buy >= vol_threshold {
                    buy_hits.push((key, buy, buy / bid_below));
                }
            }
            // Diagonal above: ask volume one tick over this level's bid volume.
            if i + 1 < keyed.len() && keyed[i + 1].0 == key + 1 {
                let ask_above = keyed[i + 1].1;
                if ask_above > 0.0 && sell >= self.ratio * ask_above && sell >= vol_threshold {
                    sell_hits.push((key, sell, sell / ask_above));
                }
            }
        }
        (self.merge_runs(&buy_hits), self.merge_runs(&sell_hits))
    }

    /// Merge adjacent imbalance levels into stacks and keep runs of `min_stack`+ levels.
    fn merge_runs(&self, hits: &[(i64, f64, f64)]) -> Vec<Stack> {
        let mut stacks = Vec::new();
        let mut run_start = 0usize;
        for i in 0..=hits.len() {
            let broke = i == hits.len() || (i > run_start && hits[i].0 != hits[i - 1].0 + 1);
            if !broke {
                continue;
            }
            let run = &hits[run_start..i];
            if run.len() >= self.min_stack {
                stacks.push(Stack {
                    low: run[0].0 as f64 * self.tick,
                    high: run[run.len() - 1].0 as f64 * self.tick,
                    levels: run.len(),
                    aggr_vol: run.iter().map(|h| h.1).sum(),
                    ratio_max: run.iter().map(|h| h.2).fold(0.0, f64::max),
                });
            }
            run_start = i;
        }
        stacks
    }

    fn publish(&self, fp: &mut Footprint, stacks: &[Stack], is_buy: bool) {
        let best = stacks
            .iter()
            .max_by(|a, b| a.aggr_vol.total_cmp(&b.aggr_vol));
        let (flag, low, high, size, vol, ratio) = if is_buy {
            (
                BUY_STACK,
                BUY_STACK_LOW,
                BUY_STACK_HIGH,
                BUY_STACK_SIZE,
                BUY_STACK_VOL,
                BUY_STACK_RATIO_MAX,
            )
        } else {
            (
                SELL_STACK,
                SELL_STACK_LOW,
                SELL_STACK_HIGH,
                SELL_STACK_SIZE,
                SELL_STACK_VOL,
                SELL_STACK_RATIO_MAX,
            )
        };
        fp.values
            .insert(flag.to_string(), if best.is_some() { 1.0 } else { 0.0 });
        if let Some(stack) = best {
            fp.values.insert(low.to_string(), stack.low);
            fp.values.insert(high.to_string(), stack.high);
            fp.values.insert(size.to_string(), stack.levels as f64);
            fp.values.insert(vol.to_string(), stack.aggr_vol);
            fp.values.insert(ratio.to_string(), stack.ratio_max);
        }
    }

    fn plot(&mut self, fp: &mut Footprint, stacks: &[Stack], is_buy: bool) {
        let mut ordered: Vec<&Stack> = stacks.iter().collect();
        ordered.sort_by(|a, b| b.aggr_vol.total_cmp(&a.aggr_vol));
        for stack in ordered.into_iter().take(self.max_zones_per_side) {
            self.seq += 1;
            fp.plots.push(Plot::PriceBox {
                id: format!("sib_{}", self.seq),
                low: stack.low - self.tick / 2.0,
                high: stack.high + self.tick / 2.0,
                span: 1,
                color: (if is_buy { BUY_COLOR } else { SELL_COLOR }).to_string(),
                text: format!(
                    "{} imb x{} {:.0}",
                    if is_buy { "buy" } else { "sell" },
                    stack.levels,
                    stack.aggr_vol
                ),
                group: Some("imbalance".to_string()),
            });
        }
    }

    fn push_window(&mut self, fp: &Footprint) {
        let mut buckets = Vec::new();
        for lv in &fp.ladder {
            for vol in [lv.buy_vol, lv.sell_vol] {
                if vol > 0.0 {
                    let bucket = (vol.round() as usize).min(HIST_BUCKETS - 1) as u16;
                    buckets.push(bucket);
                    self.hist[bucket as usize] += 1;
                    self.hist_total += 1;
                }
            }
        }
        self.window.push_back(buckets);
        while self.window.len() > self.lookback_bars {
            if let Some(old) = self.window.pop_front() {
                for bucket in old {
                    self.hist[bucket as usize] -= 1;
                    self.hist_total -= 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::Level;

    fn fp_with_ladder(levels: &[(f64, f64, f64)]) -> Footprint {
        let ladder: Vec<Level> = levels
            .iter()
            .map(|&(price, buy_vol, sell_vol)| Level {
                price,
                buy_vol,
                sell_vol,
            })
            .collect();
        let low = ladder.first().map(|l| l.price).unwrap_or(0.0);
        let high = ladder.last().map(|l| l.price).unwrap_or(0.0);
        Footprint {
            ts_first_ns: 0,
            ts_last_ns: 1,
            open: low,
            high,
            low,
            close: high,
            volume: ladder.iter().map(|l| l.buy_vol + l.sell_vol).sum(),
            trades: 1,
            delta: ladder.iter().map(|l| l.buy_vol - l.sell_vol).sum(),
            poc: low,
            ladder,
            values: Default::default(),
            tags: Default::default(),
            plots: Vec::new(),
        }
    }

    fn indicator(ratio: f64, min_stack: i64, min_volume: f64) -> StackedImbalance {
        let mut p = Params::default();
        p.set(
            "volume_mode",
            accelerando_core::ParamValue::Str("fixed".into()),
        );
        p.set("ratio", accelerando_core::ParamValue::Float(ratio));
        p.set("min_stack", accelerando_core::ParamValue::Int(min_stack));
        p.set(
            "min_volume",
            accelerando_core::ParamValue::Float(min_volume),
        );
        StackedImbalance::from_params(&p)
    }

    #[test]
    fn detects_buy_stack_of_three_diagonal_imbalances() {
        // ask(p) >= 3 x bid(p - tick) on three consecutive levels above the bottom.
        let mut fp = fp_with_ladder(&[
            (100.00, 10.0, 20.0),
            (100.25, 90.0, 25.0), // 90 >= 3*20
            (100.50, 80.0, 20.0), // 80 >= 3*25
            (100.75, 70.0, 5.0),  // 70 >= 3*20
            (101.00, 10.0, 10.0), // 10 < 3*5 fails ratio? 10 < 15 -> no
        ]);
        indicator(3.0, 3, 0.0).on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[BUY_STACK], 1.0);
        assert_eq!(fp.values[BUY_STACK_LOW], 100.25);
        assert_eq!(fp.values[BUY_STACK_HIGH], 100.75);
        assert_eq!(fp.values[BUY_STACK_SIZE], 3.0);
        assert_eq!(fp.values[SELL_STACK], 0.0);
        assert!(fp
            .plots
            .iter()
            .any(|p| matches!(p, Plot::PriceBox { group: Some(g), .. } if g == "imbalance")));
    }

    #[test]
    fn detects_sell_stack_and_respects_min_volume() {
        // bid(p) >= 3 x ask(p + tick) on three consecutive levels below the top.
        let levels = [
            (100.00, 5.0, 70.0),  // 70 >= 3*20
            (100.25, 20.0, 80.0), // 80 >= 3*25
            (100.50, 25.0, 90.0), // 90 >= 3*30
            (100.75, 30.0, 10.0),
        ];
        let mut fp = fp_with_ladder(&levels);
        indicator(3.0, 3, 0.0).on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[SELL_STACK], 1.0);
        assert_eq!(fp.values[SELL_STACK_LOW], 100.00);
        assert_eq!(fp.values[SELL_STACK_HIGH], 100.50);

        // Volume floor above the strongest print kills the stack.
        let mut fp = fp_with_ladder(&levels);
        indicator(3.0, 3, 100.0).on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[SELL_STACK], 0.0);
    }

    #[test]
    fn ladder_gap_breaks_adjacency() {
        // Two imbalances separated by a missing level do not form a stack of 3.
        let mut fp = fp_with_ladder(&[
            (100.00, 10.0, 20.0),
            (100.25, 90.0, 25.0),
            (100.50, 80.0, 20.0),
            // gap at 100.75
            (101.00, 90.0, 5.0),
        ]);
        indicator(3.0, 3, 0.0).on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[BUY_STACK], 0.0);
    }

    #[test]
    fn percentile_threshold_tracks_recent_volumes() {
        let mut p = Params::default();
        p.set("min_volume", accelerando_core::ParamValue::Float(1.0));
        p.set("percentile", accelerando_core::ParamValue::Float(0.5));
        p.set("lookback_bars", accelerando_core::ParamValue::Int(10));
        let mut ind = StackedImbalance::from_params(&p);
        // First bar sees no history -> floor.
        let mut fp = fp_with_ladder(&[(100.00, 100.0, 100.0), (100.25, 200.0, 200.0)]);
        ind.on_footprint(&mut fp, &[]);
        assert_eq!(fp.values[VOLUME_THRESHOLD], 1.0);
        // Second bar's threshold reflects the first bar's volumes (median of 100,100,200,200).
        let mut fp2 = fp_with_ladder(&[(100.00, 100.0, 100.0), (100.25, 200.0, 200.0)]);
        ind.on_footprint(&mut fp2, &[]);
        assert!(fp2.values[VOLUME_THRESHOLD] >= 100.0);
    }
}
