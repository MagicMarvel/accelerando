//! Performance metrics computed from the trade log and equity curve.

use serde::{Deserialize, Serialize};

use crate::result::{EquityPoint, Trade};

/// Summary statistics for one backtest run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub starting_equity: f64,
    pub ending_equity: f64,
    pub net_pnl: f64,
    pub return_pct: f64,
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    /// Gross profit / gross loss.
    pub profit_factor: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    /// Average PnL per trade.
    pub expectancy: f64,
    /// Annualization-free Sharpe of per-bar equity returns.
    pub sharpe: f64,
    pub sortino: f64,
    /// Worst peak-to-trough equity drop (in currency).
    pub max_drawdown: f64,
    pub max_drawdown_pct: f64,
}

impl Metrics {
    pub fn compute(starting_equity: f64, trades: &[Trade], equity: &[EquityPoint]) -> Self {
        let mut m = Metrics {
            starting_equity,
            ending_equity: equity.last().map(|e| e.equity).unwrap_or(starting_equity),
            ..Default::default()
        };
        m.net_pnl = m.ending_equity - starting_equity;
        m.return_pct = if starting_equity != 0.0 {
            m.net_pnl / starting_equity * 100.0
        } else {
            0.0
        };

        let mut gross_profit = 0.0;
        let mut gross_loss = 0.0;
        for t in trades {
            if t.pnl >= 0.0 {
                m.wins += 1;
                gross_profit += t.pnl;
            } else {
                m.losses += 1;
                gross_loss += -t.pnl;
            }
        }
        m.trades = trades.len();
        m.win_rate = if m.trades > 0 {
            m.wins as f64 / m.trades as f64
        } else {
            0.0
        };
        m.profit_factor = if gross_loss > 0.0 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
        m.avg_win = if m.wins > 0 {
            gross_profit / m.wins as f64
        } else {
            0.0
        };
        m.avg_loss = if m.losses > 0 {
            -gross_loss / m.losses as f64
        } else {
            0.0
        };
        m.expectancy = if m.trades > 0 {
            m.net_pnl / m.trades as f64
        } else {
            0.0
        };

        // Per-bar equity returns drive Sharpe/Sortino and drawdown.
        let mut peak = starting_equity;
        let mut rets: Vec<f64> = Vec::with_capacity(equity.len());
        let mut prev = starting_equity;
        for e in equity {
            if prev != 0.0 {
                rets.push((e.equity - prev) / prev);
            }
            prev = e.equity;
            peak = peak.max(e.equity);
            let dd = e.equity - peak;
            if dd < m.max_drawdown {
                m.max_drawdown = dd;
            }
        }
        m.max_drawdown_pct = if peak != 0.0 {
            m.max_drawdown / peak * 100.0
        } else {
            0.0
        };

        m.sharpe = sharpe(&rets);
        m.sortino = sortino(&rets);
        m
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn sharpe(rets: &[f64]) -> f64 {
    if rets.len() < 2 {
        return 0.0;
    }
    let mu = mean(rets);
    let var = rets.iter().map(|r| (r - mu).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
    let sd = var.sqrt();
    if sd > 0.0 {
        mu / sd * (rets.len() as f64).sqrt()
    } else {
        0.0
    }
}

fn sortino(rets: &[f64]) -> f64 {
    if rets.len() < 2 {
        return 0.0;
    }
    let mu = mean(rets);
    let downside = rets
        .iter()
        .filter(|r| **r < 0.0)
        .map(|r| r.powi(2))
        .sum::<f64>();
    let dd = (downside / rets.len() as f64).sqrt();
    if dd > 0.0 {
        mu / dd * (rets.len() as f64).sqrt()
    } else {
        0.0
    }
}
