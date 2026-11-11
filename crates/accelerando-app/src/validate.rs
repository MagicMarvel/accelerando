//! Out-of-sample and robustness checks that should run before anyone believes a backtest.
//!
//! All checks reuse full-history runs and slice the *trade list* by entry time, so indicator and
//! strategy state stay continuous across window boundaries (no artificial warm-up at each split).
//!
//! * [`split_report`] — in-sample / out-of-sample split; with several candidates, the IS winner's
//!   OOS result plus the IS→OOS rank correlation across all candidates.
//! * [`walk_forward`] — K equal-time folds: pick the best candidate on fold k-1, score it on
//!   fold k. Only the stitched OOS results count.
//! * [`cost_sensitivity`] — rerun one candidate at several slippage levels.
//! * [`random_baseline`] — same trade count, holding times and session minutes, random entries
//!   and directions. Answers "is this better than chance on this data?"

use std::collections::HashMap;

use accelerando_core::{
    run_prepared_backtest, BacktestResult, BarCtx, BrokerConfig, EntryIntent, Footprint,
    PreparedBacktestData, Strategy, StrategyOutput, Trade, TradeSide,
};
use rayon::prelude::*;
use serde::Serialize;

use crate::stats::{fmt_pf, spearman, TradeStats};

/// One evaluated parameter set.
pub struct Candidate {
    pub label: String,
    pub params: accelerando_core::Params,
    pub result: BacktestResult,
}

fn time_bounds(data: &PreparedBacktestData) -> (i64, i64) {
    let first = data.footprints.first().map(|f| f.ts_first_ns).unwrap_or(0);
    let last = data.footprints.last().map(|f| f.ts_last_ns).unwrap_or(0) + 1;
    (first, last)
}

#[derive(Clone, Debug, Serialize)]
pub struct SplitReport {
    pub split_fraction: f64,
    pub split_ts_ns: i64,
    pub chosen: String,
    pub in_sample: TradeStats,
    pub out_of_sample: TradeStats,
    /// Spearman correlation of candidate net PnL in IS vs OOS (grids only).
    pub rank_correlation: Option<f64>,
    /// Share of candidates that were profitable OOS.
    pub oos_profitable_share: Option<f64>,
}

/// IS/OOS split at `fraction` of the data's time span. With several candidates, the chosen one is
/// the in-sample net-PnL leader.
pub fn split_report(
    data: &PreparedBacktestData,
    candidates: &[Candidate],
    fraction: f64,
) -> Option<SplitReport> {
    if candidates.is_empty() {
        return None;
    }
    let (first, last) = time_bounds(data);
    let split = first + ((last - first) as f64 * fraction.clamp(0.05, 0.95)) as i64;
    let is: Vec<TradeStats> = candidates
        .iter()
        .map(|c| TradeStats::window(&c.result.trades, first, split))
        .collect();
    let oos: Vec<TradeStats> = candidates
        .iter()
        .map(|c| TradeStats::window(&c.result.trades, split, last))
        .collect();
    let best = (0..candidates.len())
        .max_by(|&a, &b| is[a].net_pnl.total_cmp(&is[b].net_pnl))
        .unwrap_or(0);
    let multi = candidates.len() > 1;
    Some(SplitReport {
        split_fraction: fraction,
        split_ts_ns: split,
        chosen: candidates[best].label.clone(),
        in_sample: is[best].clone(),
        out_of_sample: oos[best].clone(),
        rank_correlation: if multi {
            spearman(
                &is.iter().map(|s| s.net_pnl).collect::<Vec<_>>(),
                &oos.iter().map(|s| s.net_pnl).collect::<Vec<_>>(),
            )
        } else {
            None
        },
        oos_profitable_share: multi.then(|| {
            oos.iter().filter(|s| s.net_pnl > 0.0).count() as f64 / oos.len() as f64
        }),
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct WalkForwardFold {
    pub fold: usize,
    pub from_ts_ns: i64,
    pub to_ts_ns: i64,
    pub chosen: String,
    pub selection: TradeStats,
    pub out_of_sample: TradeStats,
}

#[derive(Clone, Debug, Serialize)]
pub struct WalkForwardReport {
    pub folds: Vec<WalkForwardFold>,
    /// Stitched OOS trades of every fold's chosen candidate.
    pub combined_oos: TradeStats,
}

/// Rolling selection: choose on fold k-1 (net PnL, at least `min_trades`), evaluate on fold k.
pub fn walk_forward(
    data: &PreparedBacktestData,
    candidates: &[Candidate],
    folds: usize,
    min_trades: usize,
) -> Option<WalkForwardReport> {
    let folds = folds.max(2);
    if candidates.len() < 2 {
        return None;
    }
    let (first, last) = time_bounds(data);
    let width = (last - first) / folds as i64;
    let edge = |k: usize| if k == folds { last } else { first + width * k as i64 };
    let mut out = Vec::new();
    let mut stitched: Vec<&Trade> = Vec::new();
    for k in 1..folds {
        let (sel_from, sel_to) = (edge(k - 1), edge(k));
        let (oos_from, oos_to) = (edge(k), edge(k + 1));
        let scored: Vec<TradeStats> = candidates
            .iter()
            .map(|c| TradeStats::window(&c.result.trades, sel_from, sel_to))
            .collect();
        let best = (0..candidates.len())
            .filter(|&i| scored[i].trades >= min_trades)
            .max_by(|&a, &b| scored[a].net_pnl.total_cmp(&scored[b].net_pnl));
        let Some(best) = best else { continue };
        let trades = &candidates[best].result.trades;
        stitched.extend(
            trades
                .iter()
                .filter(|t| t.entry_ts_ns >= oos_from && t.entry_ts_ns < oos_to),
        );
        out.push(WalkForwardFold {
            fold: k,
            from_ts_ns: oos_from,
            to_ts_ns: oos_to,
            chosen: candidates[best].label.clone(),
            selection: scored[best].clone(),
            out_of_sample: TradeStats::window(trades, oos_from, oos_to),
        });
    }
    stitched.sort_by_key(|t| t.entry_ts_ns);
    Some(WalkForwardReport {
        folds: out,
        combined_oos: TradeStats::from_trades(stitched),
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct CostPoint {
    pub slippage_ticks: f64,
    pub stats: TradeStats,
}

/// Rerun `build()` at each slippage level.
pub fn cost_sensitivity(
    data: &PreparedBacktestData,
    build: &(dyn Fn() -> Box<dyn Strategy> + Sync),
    base: BrokerConfig,
    slippages: &[f64],
) -> Vec<CostPoint> {
    slippages
        .par_iter()
        .map(|&slippage_ticks| {
            let cfg = BrokerConfig {
                slippage_ticks,
                ..base
            };
            let result = run_prepared_backtest(data, build(), cfg, false);
            CostPoint {
                slippage_ticks,
                stats: TradeStats::from_trades(&result.trades),
            }
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct RandomBaseline {
    pub runs: usize,
    pub strategy_net: f64,
    pub random_mean: f64,
    pub random_p05: f64,
    pub random_p50: f64,
    pub random_p95: f64,
    /// Share of random runs the strategy beat (0..1). Below ~0.95 the edge is not distinguishable
    /// from chance on this sample.
    pub beat_share: f64,
}

/// Random-entry baseline matched to the strategy's own trade count, holding-time distribution and
/// entry-minute range. Entries are market orders with a random direction; exits are time-based.
pub fn random_baseline(
    data: &PreparedBacktestData,
    strategy_trades: &[Trade],
    cfg: BrokerConfig,
    runs: usize,
    seed: u64,
) -> Option<RandomBaseline> {
    let fps = &data.footprints;
    if strategy_trades.is_empty() || fps.len() < 10 || runs == 0 {
        return None;
    }
    let bar_of = |ts: i64| fps.partition_point(|f| f.ts_last_ns < ts).min(fps.len() - 1);
    let holds: Vec<usize> = strategy_trades
        .iter()
        .map(|t| bar_of(t.exit_ts_ns).saturating_sub(bar_of(t.entry_ts_ns)).max(1))
        .collect();
    let minutes: Vec<u32> = strategy_trades
        .iter()
        .map(|t| accelerando_core::market_time::eastern_day_minute(t.entry_ts_ns).1)
        .collect();
    let (lo, hi) = (
        *minutes.iter().min()?,
        *minutes.iter().max()?,
    );
    // Signal bar = one bar before the fill bar.
    let eligible: Vec<usize> = (0..fps.len() - 1)
        .filter(|&i| {
            let m = fps[i + 1].eastern_day_minute().1;
            m >= lo && m <= hi
        })
        .collect();
    if eligible.is_empty() {
        return None;
    }
    let n = strategy_trades.len();
    let mut nets: Vec<f64> = (0..runs)
        .into_par_iter()
        .map(|r| {
            let mut rng = XorShift::new(seed ^ (r as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut picks: Vec<usize> = (0..n * 3)
                .map(|_| eligible[rng.below(eligible.len())])
                .collect();
            picks.sort_unstable();
            picks.dedup();
            let mut schedule = HashMap::new();
            let mut busy_until = 0usize;
            for &i in &picks {
                if schedule.len() >= n {
                    break;
                }
                if i < busy_until {
                    continue;
                }
                let hold = holds[rng.below(holds.len())];
                let dir = if rng.below(2) == 0 { 1 } else { -1 };
                schedule.insert(i, (dir, hold));
                busy_until = i + hold + 2;
            }
            let strat = RandomEntry {
                schedule,
                hold: 0,
            };
            let result = run_prepared_backtest(data, Box::new(strat), cfg, false);
            result.trades.iter().map(|t| t.pnl).sum::<f64>()
        })
        .collect();
    nets.sort_by(f64::total_cmp);
    let strategy_net: f64 = strategy_trades.iter().map(|t| t.pnl).sum();
    let q = |p: f64| nets[((nets.len() - 1) as f64 * p).round() as usize];
    Some(RandomBaseline {
        runs,
        strategy_net,
        random_mean: nets.iter().sum::<f64>() / nets.len() as f64,
        random_p05: q(0.05),
        random_p50: q(0.5),
        random_p95: q(0.95),
        beat_share: nets.iter().filter(|&&x| strategy_net > x).count() as f64 / nets.len() as f64,
    })
}

struct RandomEntry {
    schedule: HashMap<usize, (i32, usize)>,
    hold: usize,
}

impl Strategy for RandomEntry {
    fn on_footprint(&mut self, _fp: &Footprint, ctx: &BarCtx, out: &mut StrategyOutput) {
        let i = ctx.bar;
        if let Some(pos) = ctx.open_position.as_ref() {
            if pos.bars_held >= self.hold {
                out.orders.exit_position();
            }
            return;
        }
        if let Some(&(dir, hold)) = self.schedule.get(&i) {
            self.hold = hold;
            let side = if dir > 0 { TradeSide::Long } else { TradeSide::Short };
            out.orders.replace(EntryIntent::market(side, 1.0));
        }
    }
}

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Everything `--validate` computes.
#[derive(Clone, Debug, Serialize)]
pub struct ValidationReport {
    pub split: Option<SplitReport>,
    pub walk_forward: Option<WalkForwardReport>,
    pub costs: Vec<CostPoint>,
    pub random: Option<RandomBaseline>,
}

impl ValidationReport {
    pub fn print(&self) {
        println!("\n================ validation ================");
        if let Some(s) = &self.split {
            println!(
                "IS/OOS split at {:.0}% of time  (chosen: {})",
                s.split_fraction * 100.0,
                s.chosen
            );
            println!("          {}", TradeStats::HEADER);
            println!("  IS      {}", s.in_sample.row());
            println!("  OOS     {}", s.out_of_sample.row());
            if let Some(rho) = s.rank_correlation {
                println!("  IS→OOS rank correlation across candidates: {rho:+.2}");
            }
            if let Some(share) = s.oos_profitable_share {
                println!("  candidates profitable OOS: {:.0}%", share * 100.0);
            }
        }
        if let Some(wf) = &self.walk_forward {
            println!("\nwalk-forward ({} OOS folds; choose on previous fold)", wf.folds.len());
            for f in &wf.folds {
                println!(
                    "  fold {}  chosen {:<28} sel net {:>9.2}  OOS {}",
                    f.fold,
                    f.chosen,
                    f.selection.net_pnl,
                    f.out_of_sample.row()
                );
            }
            println!("  stitched OOS      {}", wf.combined_oos.row());
        }
        if !self.costs.is_empty() {
            println!("\ncost sensitivity (slippage ticks per side)");
            for c in &self.costs {
                println!(
                    "  slip {:>3.1}  net {:>10.2}  PF {:>5}  trades {}",
                    c.slippage_ticks,
                    c.stats.net_pnl,
                    fmt_pf(c.stats.profit_factor),
                    c.stats.trades
                );
            }
        }
        if let Some(r) = &self.random {
            println!(
                "\nrandom-entry baseline ({} runs, matched count/holding/minutes)",
                r.runs
            );
            println!(
                "  strategy {:.2}  random mean {:.2}  p5 {:.2}  p50 {:.2}  p95 {:.2}",
                r.strategy_net, r.random_mean, r.random_p05, r.random_p50, r.random_p95
            );
            println!(
                "  strategy beats {:.0}% of random runs{}",
                r.beat_share * 100.0,
                if r.beat_share < 0.95 {
                    "  → NOT distinguishable from chance"
                } else {
                    ""
                }
            );
        }
        println!("============================================");
    }
}
