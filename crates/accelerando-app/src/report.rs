//! Detailed terminal report for the run selected by net PnL.

use std::collections::BTreeMap;

use accelerando_core::market_time::civil_from_days;
use accelerando_core::result::EquityPoint;
use accelerando_core::{TimeZone, Trade, TradeReason};

use crate::{terminal::Palette, RunOptions, RunOutcome, TradeStats};

pub(crate) fn print(run: &RunOutcome, options: &RunOptions, colors: Palette) {
    let result = &run.result;
    let m = &result.metrics;
    println!(
        "\n{}",
        colors.heading(format_args!("Detailed report: {}", run.label))
    );
    if options.grid {
        println!("Selected by net PnL | {}", crate::fmt_params(&run.params));
    }
    println!(
        "Equity {:.2} -> {:.2} | net {}",
        m.starting_equity,
        m.ending_equity,
        colors.pnl(
            m.net_pnl,
            format_args!("{:+.2} ({:+.2}%)", m.net_pnl, m.return_pct)
        )
    );
    println!("Sharpe {:.3} | Sortino {:.3}", m.sharpe, m.sortino);
    if let (Some(first), Some(last)) = (result.equity.first(), result.equity.last()) {
        println!(
            "Equity sample (UTC) {} -> {} | {} marks",
            timestamp(first.ts_ns),
            timestamp(last.ts_ns),
            result.equity.len()
        );
        print_drawdown(m.starting_equity, &result.equity, colors);
    }

    if result.trades.is_empty() {
        println!("No closed trades.");
        return;
    }
    let mut trades: Vec<_> = result.trades.iter().collect();
    // Preserve broker order for closes with the same timestamp.
    trades.sort_by_key(|t| t.exit_ts_ns);
    let mut pnl: Vec<_> = trades.iter().map(|t| t.pnl).collect();
    pnl.sort_unstable_by(f64::total_cmp);
    let losses = pnl.partition_point(|&p| p < 0.0);
    let non_wins = pnl.partition_point(|&p| p <= 0.0);
    let wins = pnl.len() - non_wins;
    let gross_profit: f64 = pnl[non_wins..].iter().sum();
    let gross_loss: f64 = -pnl[..losses].iter().sum::<f64>();
    let avg_win = gross_profit / wins.max(1) as f64;
    let avg_loss = gross_loss / losses.max(1) as f64;
    let stats = TradeStats::from_trades(trades.iter().copied());
    let (win_streak, loss_streak) = longest_streaks(&trades);
    let contracts: f64 = trades.iter().map(|t| t.qty).sum();

    println!("\n{}", colors.heading("Closed trades (PnL after costs)"));
    println!(
        "{} trades | {} | {} | flat {} | PF {}",
        pnl.len(),
        colors.profit(format_args!(
            "won {wins} ({:.2}%)",
            share(wins as f64, pnl.len() as f64)
        )),
        colors.loss(format_args!(
            "lost {losses} ({:.2}%)",
            share(losses as f64, pnl.len() as f64)
        )),
        non_wins - losses,
        crate::stats::fmt_pf(stats.profit_factor)
    );
    println!(
        "{} | {}",
        colors.profit(format_args!(
            "Gross profit {gross_profit:.2} (avg {avg_win:.2})"
        )),
        colors.loss(format_args!(
            "gross loss {gross_loss:.2} (avg {:.2})",
            -avg_loss
        ))
    );
    println!(
        "Avg trade {} | avg win / avg loss {} | closed-trade max DD {}",
        colors.pnl(stats.avg_trade, format_args!("{:+.2}", stats.avg_trade)),
        if losses == 0 || wins == 0 {
            "-".into()
        } else {
            format!("{:.2}", avg_win / avg_loss)
        },
        colors.risk(format_args!("{:.2}", stats.max_drawdown))
    );
    println!(
        "Longest win/loss streak {}/{} | avg MAE {:.2} ({:.2} ticks)",
        colors.profit(win_streak),
        colors.loss(loss_streak),
        m.avg_max_adverse_excursion,
        m.avg_max_adverse_ticks
    );
    println!(
        "Commission {:.2} | {:.2} round-trip contracts | slippage {:.2} ticks/side",
        2.0 * options.commission * contracts,
        contracts,
        options.slippage_ticks
    );
    println!(
        "Peak concurrent contracts {:.2} (gross, from closed trade lifetimes)",
        peak_contracts(&trades)
    );

    println!(
        "\n{}",
        colors.heading("Trade PnL percentiles (account currency, linear interpolation)")
    );
    print_percentiles(&pnl, &[0, 5, 25, 50, 75, 95, 100], Some(colors));
    let mut minutes: Vec<_> = trades
        .iter()
        .map(|t| (i128::from(t.exit_ts_ns) - i128::from(t.entry_ts_ns)) as f64 / 60e9)
        .collect();
    minutes.sort_unstable_by(f64::total_cmp);
    println!(
        "Holding time (minutes, mean {:.2})",
        minutes.iter().sum::<f64>() / minutes.len() as f64
    );
    print_percentiles(&minutes, &[0, 25, 50, 75, 95, 100], None);
    if m.starting_equity > 0.0 {
        print_distribution(&pnl, m.starting_equity, colors);
    }
    print_concentration(&pnl, wins, losses, gross_profit, gross_loss, colors);
    print_breakdowns(&trades, colors);
}

fn share(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        0.0
    } else {
        100.0 * numerator / denominator
    }
}

fn timestamp(ns: i64) -> String {
    let (day, minute) = TimeZone::UTC.day_minute(ns);
    let (year, month, day) = civil_from_days(day);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        minute / 60,
        minute % 60
    )
}

fn print_drawdown(starting_equity: f64, equity: &[EquityPoint], colors: Palette) {
    let mut peak = EquityPoint {
        equity: starting_equity,
        drawdown: 0.0,
        ..equity[0]
    };
    let mut worst = (peak, 0, 0.0);
    let mut max_pct: f64 = 0.0;
    for (index, &point) in equity.iter().enumerate() {
        if point.equity >= peak.equity {
            peak = point;
        }
        let drop = peak.equity - point.equity;
        if drop > worst.2 {
            worst = (peak, index, drop);
        }
        if peak.equity > 0.0 {
            max_pct = max_pct.max(share(drop, peak.equity));
        }
    }
    let (from, bottom_index, drop) = worst;
    let current = equity.last().unwrap().equity;
    let relative = if peak.equity > 0.0 {
        format!("{max_pct:.2}%")
    } else {
        "n/a".into()
    };
    println!(
        "{}",
        colors.risk(format_args!(
            "Max equity DD {drop:.2} | max DD from peak {relative} | ending DD {:.2}",
            peak.equity - current
        ))
    );
    if drop > 0.0 {
        let bottom = equity[bottom_index];
        let recovered = equity[bottom_index + 1..]
            .iter()
            .find(|p| p.equity >= from.equity)
            .map(|p| timestamp(p.ts_ns))
            .unwrap_or_else(|| "unrecovered".into());
        println!(
            "Max cash DD (UTC) {} @ {:.2} -> {} @ {:.2}",
            timestamp(from.ts_ns),
            from.equity,
            timestamp(bottom.ts_ns),
            bottom.equity
        );
        println!("Recovery (UTC) {recovered}");
    }
}

fn longest_streaks(trades: &[&Trade]) -> (usize, usize) {
    let (mut win, mut loss, mut max_win, mut max_loss) = (0, 0, 0, 0);
    for trade in trades {
        win = if trade.pnl > 0.0 { win + 1 } else { 0 };
        loss = if trade.pnl < 0.0 { loss + 1 } else { 0 };
        max_win = max_win.max(win);
        max_loss = max_loss.max(loss);
    }
    (max_win, max_loss)
}

fn peak_contracts(trades: &[&Trade]) -> f64 {
    let mut changes = BTreeMap::<_, f64>::new();
    for trade in trades {
        *changes.entry(trade.entry_ts_ns).or_default() += trade.qty;
        *changes.entry(trade.exit_ts_ns).or_default() -= trade.qty;
    }
    let mut open = 0.0;
    let mut peak: f64 = 0.0;
    for change in changes.into_values() {
        open += change;
        peak = peak.max(open);
    }
    peak
}

fn print_percentiles(sorted: &[f64], percentiles: &[usize], pnl_colors: Option<Palette>) {
    for (column, &p) in percentiles.iter().enumerate() {
        if column == 4 {
            println!();
        }
        let index = (sorted.len() - 1) as f64 * p as f64 / 100.0;
        let lo = sorted[index.floor() as usize];
        let hi = sorted[index.ceil() as usize];
        let value = lo + (hi - lo) * index.fract();
        if let Some(colors) = pnl_colors {
            print!(
                "  P{p:<3} {}",
                colors.pnl(value, format_args!("{value:>10.2}"))
            );
        } else {
            print!("  P{p:<3} {value:>10.2}");
        }
    }
    println!();
}

fn print_distribution(pnl: &[f64], starting_equity: f64, colors: Palette) {
    let thresholds = [0.10, 0.25, 0.50, 1.00];
    let cutoffs = thresholds.map(|pct| starting_equity * pct / 100.0);
    let mut counts = [[0; 2]; 5];
    let mut sums = [[0.0; 2]; 5];
    for &p in pnl.iter().filter(|&&p| p != 0.0) {
        let band = cutoffs.partition_point(|&limit| limit <= p.abs());
        let side = usize::from(p > 0.0);
        counts[band][side] += 1;
        sums[band][side] += p;
    }
    println!("\n{}", colors.heading("Trade PnL distribution"));
    println!("Absolute PnL as % of initial equity. Upper bounds excluded.");
    println!(
        "{:>14} {:>8} {:>8} {:>12} {:>8} {:>8} {:>12}",
        "size", "losses", "% all", "loss PnL", "wins", "% all", "win PnL"
    );
    let mut lower = 0.0;
    for (i, upper) in thresholds.into_iter().chain([f64::INFINITY]).enumerate() {
        let label = if upper.is_finite() {
            format!("{lower:.2}-{upper:.2}%")
        } else {
            format!(">={lower:.2}%")
        };
        let [losses, wins] = counts[i];
        let [loss_pnl, win_pnl] = sums[i];
        println!(
            "{label:>14} {} {}",
            colors.loss(format_args!(
                "{losses:>8} {:>7.2}% {loss_pnl:>12.2}",
                share(losses as f64, pnl.len() as f64)
            )),
            colors.profit(format_args!(
                "{wins:>8} {:>7.2}% {win_pnl:>12.2}",
                share(wins as f64, pnl.len() as f64)
            ))
        );
        lower = upper;
    }
}

fn print_concentration(
    pnl: &[f64],
    wins: usize,
    losses: usize,
    gross_profit: f64,
    gross_loss: f64,
    colors: Palette,
) {
    println!(
        "\n{}",
        colors.heading("PnL concentration (ranked by PnL, requested % of all trades rounded up)")
    );
    println!("Profit and loss shares use gross profit and gross loss respectively.");
    println!(
        "{:>8} {:>7} {:>12} {:>10} {:>12} {:>10} {:>14}",
        "tail", "trades", "top profits", "% profits", "worst losses", "% losses", "net ex top wins"
    );
    for pct in [1, 5, 10, 20] {
        let count = (pnl.len() * pct).div_ceil(100);
        let top: f64 = pnl.iter().rev().take(count.min(wins)).sum();
        let bottom: f64 = -pnl.iter().take(count.min(losses)).sum::<f64>();
        let net_without_top = gross_profit - gross_loss - top;
        println!(
            "{pct:>7}% {count:>7} {} {} {}",
            colors.profit(format_args!(
                "{top:>12.2} {:>9.2}%",
                share(top, gross_profit)
            )),
            colors.loss(format_args!(
                "{:>12.2} {:>9.2}%",
                -bottom,
                share(bottom, gross_loss)
            )),
            colors.pnl(net_without_top, format_args!("{net_without_top:>14.2}"))
        );
    }
}

fn print_breakdowns(trades: &[&Trade], colors: Palette) {
    let mut days: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    let mut months: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for &trade in trades {
        let (day, _) = TimeZone::UTC.day_minute(trade.exit_ts_ns);
        let (year, month, _) = civil_from_days(day);
        let (pnl, count) = days.entry(day).or_default();
        *pnl += trade.pnl;
        *count += 1;
        months.entry((year, month)).or_default().push(trade);
    }
    let winning_days = days.values().filter(|(pnl, _)| *pnl > 0.0).count();
    let losing_days = days.values().filter(|(pnl, _)| *pnl < 0.0).count();
    println!(
        "\nUTC close days {} | profitable {} | losing {} | flat {} | {:.2} trades/day",
        days.len(),
        winning_days,
        losing_days,
        days.len() - winning_days - losing_days,
        trades.len() as f64 / days.len() as f64
    );
    let compare_pnl =
        |&(_, (a, _)): &(&i64, &(f64, usize)), &(_, (b, _)): &(&i64, &(f64, usize))| a.total_cmp(b);
    for (heading, row) in [
        ("Best", days.iter().max_by(compare_pnl)),
        ("Worst", days.iter().min_by(compare_pnl)),
    ] {
        let (&day, &(pnl, count)) = row.expect("at least one closed trade");
        let (year, month, day) = civil_from_days(day);
        println!(
            "{heading} close day {year:04}-{month:02}-{day:02} {} ({count} trades)",
            colors.pnl(pnl, format_args!("{pnl:+.2}"))
        );
    }

    println!(
        "\n{}",
        colors.heading("Breakdown (months by UTC close, win% counts strictly positive trades)")
    );
    println!("{:<13} {}", "group", TradeStats::HEADER);
    for (label, dir) in [("long", 1), ("short", -1)] {
        let stats = TradeStats::from_trades(trades.iter().copied().filter(|t| t.dir == dir));
        println!(
            "{} {}",
            colors.pnl(stats.net_pnl, format_args!("{label:<13}")),
            stats.row()
        );
    }
    for ((year, month), trades) in months {
        let label = format!("{year:04}-{month:02}");
        let stats = TradeStats::from_trades(trades);
        println!(
            "{} {}",
            colors.pnl(stats.net_pnl, format_args!("{label:<13}")),
            stats.row()
        );
    }
    for (label, reason) in [
        ("signal", TradeReason::Signal),
        ("stop_loss", TradeReason::StopLoss),
        ("take_profit", TradeReason::TakeProfit),
        ("end_of_data", TradeReason::EndOfData),
    ] {
        let stats = TradeStats::from_trades(trades.iter().copied().filter(|t| t.reason == reason));
        if stats.trades > 0 {
            println!(
                "{} {}",
                colors.pnl(stats.net_pnl, format_args!("{label:<13}")),
                stats.row()
            );
        }
    }
}
