//! Trade-list statistics over arbitrary time windows.

use accelerando_core::Trade;
use serde::Serialize;

/// Summary of the trades whose *entry* falls inside a window.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TradeStats {
    pub trades: usize,
    pub net_pnl: f64,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub avg_trade: f64,
    /// Peak-to-trough drawdown of the cumulative trade PnL (closed trades only).
    pub max_drawdown: f64,
}

impl TradeStats {
    pub fn from_trades<'a>(trades: impl IntoIterator<Item = &'a Trade>) -> Self {
        let mut s = TradeStats::default();
        let (mut gross_win, mut gross_loss, mut wins) = (0.0, 0.0, 0usize);
        let (mut cum, mut peak) = (0.0f64, 0.0f64);
        for t in trades {
            s.trades += 1;
            s.net_pnl += t.pnl;
            if t.pnl > 0.0 {
                wins += 1;
                gross_win += t.pnl;
            } else {
                gross_loss -= t.pnl;
            }
            cum += t.pnl;
            peak = peak.max(cum);
            s.max_drawdown = s.max_drawdown.max(peak - cum);
        }
        if s.trades > 0 {
            s.win_rate = wins as f64 / s.trades as f64 * 100.0;
            s.avg_trade = s.net_pnl / s.trades as f64;
        }
        s.profit_factor = if gross_loss > 0.0 {
            gross_win / gross_loss
        } else if gross_win > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
        s
    }

    /// Trades entered in `[from_ns, to_ns)`.
    pub fn window(trades: &[Trade], from_ns: i64, to_ns: i64) -> Self {
        Self::from_trades(
            trades
                .iter()
                .filter(|t| t.entry_ts_ns >= from_ns && t.entry_ts_ns < to_ns),
        )
    }

    pub fn row(&self) -> String {
        format!(
            "{:>6} {:>11.2} {:>7.1}% {:>6} {:>9.2} {:>10.2}",
            self.trades,
            self.net_pnl,
            self.win_rate,
            fmt_pf(self.profit_factor),
            self.avg_trade,
            self.max_drawdown
        )
    }

    pub const HEADER: &'static str = "trades         net    win%     PF  avg/trade      maxDD";
}

pub fn fmt_pf(pf: f64) -> String {
    if pf.is_infinite() {
        "inf".into()
    } else {
        format!("{pf:.2}")
    }
}

/// Spearman rank correlation (average ranks for ties). `None` for fewer than 3 points or a
/// constant series.
pub fn spearman(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() != b.len() || a.len() < 3 {
        return None;
    }
    let ra = ranks(a);
    let rb = ranks(b);
    let n = ra.len() as f64;
    let ma = ra.iter().sum::<f64>() / n;
    let mb = rb.iter().sum::<f64>() / n;
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (x, y) in ra.iter().zip(&rb) {
        cov += (x - ma) * (y - mb);
        va += (x - ma).powi(2);
        vb += (y - mb).powi(2);
    }
    (va > 0.0 && vb > 0.0).then(|| cov / (va.sqrt() * vb.sqrt()))
}

fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&i, &j| v[i].total_cmp(&v[j]));
    let mut out = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1;
        }
        let rank = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            out[idx[k]] = rank;
        }
        i = j + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::TradeReason;

    fn t(ts: i64, pnl: f64) -> Trade {
        Trade {
            entry_ts_ns: ts,
            exit_ts_ns: ts + 1,
            dir: 1,
            qty: 1.0,
            entry_px: 0.0,
            exit_px: 0.0,
            stop: None,
            target: None,
            max_adverse_excursion: 0.0,
            max_adverse_ticks: 0.0,
            pnl,
            reason: TradeReason::Signal,
            label: None,
        }
    }

    #[test]
    fn window_stats() {
        let trades = vec![t(0, 100.0), t(1, -50.0), t(2, -50.0), t(3, 25.0), t(9, 1000.0)];
        let s = TradeStats::window(&trades, 0, 5);
        assert_eq!(s.trades, 4);
        assert_eq!(s.net_pnl, 25.0);
        assert_eq!(s.win_rate, 50.0);
        assert_eq!(s.profit_factor, 1.25);
        assert_eq!(s.max_drawdown, 100.0);
    }

    #[test]
    fn spearman_basic() {
        let up = spearman(&[1.0, 2.0, 3.0], &[10.0, 20.0, 30.0]).unwrap();
        let down = spearman(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]).unwrap();
        assert!((up - 1.0).abs() < 1e-12 && (down + 1.0).abs() < 1e-12);
        assert_eq!(spearman(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), None);
    }
}
