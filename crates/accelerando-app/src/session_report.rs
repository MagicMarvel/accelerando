//! Entry-time performance by host-defined trading sessions and half hour.

use std::collections::BTreeSet;

use accelerando_core::{in_clock_window, TimeZone, Trade};

use crate::{stats::fmt_pf, terminal::Palette, TradeStats};

const SLOTS: usize = 48;
const MINUTES_PER_SLOT: u32 = 30;
const MINUTES_PER_DAY: u32 = SLOTS as u32 * MINUTES_PER_SLOT;

/// Market hours supplied by the application hosting the runner.
pub struct SessionReportOptions {
    pub timezone: TimeZone,
    pub timezone_label: &'static str,
    /// Local minute of day at which trading dates roll over.
    pub day_starts_at: u32,
    /// (Name, start minute, end minute), in local time. Windows may wrap midnight.
    pub windows: &'static [(&'static str, u32, u32)],
}

type Entry<'a> = (i64, u32, &'a Trade);

pub(crate) fn print(trades: &[Trade], colors: Palette, options: &SessionReportOptions) {
    let rollover = options.day_starts_at % MINUTES_PER_DAY;
    let open_slot = (rollover / MINUTES_PER_SLOT) as usize;
    let mut slots: [Vec<Entry<'_>>; SLOTS] = std::array::from_fn(|_| Vec::new());
    for trade in trades {
        let (civil_day, minute) = options.timezone.day_minute(trade.entry_ts_ns);
        let local_slot = (minute / MINUTES_PER_SLOT) as usize;
        let slot = (local_slot + SLOTS - open_slot) % SLOTS;
        let trading_day = civil_day + i64::from(rollover > 0 && minute >= rollover);
        slots[slot].push((trading_day, minute, trade));
    }

    let sessions: Vec<_> = options
        .windows
        .iter()
        .map(|&(name, start, end)| {
            let (stats, days) = summarize(
                slots
                    .iter()
                    .flatten()
                    .filter(|entry| in_clock_window(entry.1, start, end)),
            );
            let label = format!("{name} {}-{}", clock(start), clock(end));
            (label, stats, days)
        })
        .collect();
    let half_hours: [_; SLOTS] = std::array::from_fn(|i| {
        let (stats, days) = summarize(slots[i].iter());
        let start = ((i + open_slot) % SLOTS) as u32 * MINUTES_PER_SLOT;
        let end = (start + MINUTES_PER_SLOT) % MINUTES_PER_DAY;
        (format!("{}-{}", clock(start), clock(end)), stats, days)
    });

    let zone = options.timezone_label;
    println!("\n{}", colors.heading("Trading session analysis"));
    println!("Time zone {zone}. Net PnL grouped by entry time.");
    println!(
        "Days counts trading dates with entries ({} {zone} rollover).",
        clock(rollover)
    );
    println!(
        "{}  {}  {:>5}  {:>11}  {:>6}  {:>5}  {:>10}",
        "Session             ", "Trades", "Days", "Net", "Win%", "PF", "Avg/trade"
    );
    for (label, stats, days) in &sessions {
        print_row(label, stats, *days, colors);
    }

    println!(
        "\n{}",
        colors.heading(format_args!("Half-hour entries ({zone})"))
    );
    println!(
        "{}  {}  {:>5}  {:>11}  {:>6}  {:>5}  {:>10}",
        "Window              ", "Trades", "Days", "Net", "Win%", "PF", "Avg/trade"
    );
    for (label, stats, days) in &half_hours {
        print_row(label, stats, *days, colors);
    }
    print_best("session", &sessions, colors);
    print_best("half hour", &half_hours, colors);
}

fn summarize<'a>(entries: impl Iterator<Item = &'a Entry<'a>>) -> (TradeStats, usize) {
    let mut days = BTreeSet::new();
    let stats = TradeStats::from_trades(entries.map(|&(day, _, trade)| {
        days.insert(day);
        trade
    }));
    (stats, days.len())
}

fn clock(minute: u32) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

fn print_row(label: &str, stats: &TradeStats, days: usize, colors: Palette) {
    if stats.trades == 0 {
        println!(
            "{label:<20}  {:>6}  {days:>5}  {:>11}  {:>6}  {:>5}  {:>10}",
            0, "-", "-", "-", "-"
        );
    } else {
        println!(
            "{label:<20}  {:>6}  {days:>5}  {}  {:>5.1}%  {:>5}  {}",
            stats.trades,
            colors.pnl(stats.net_pnl, format_args!("{:>+11.2}", stats.net_pnl)),
            stats.win_rate,
            fmt_pf(stats.profit_factor),
            colors.pnl(stats.avg_trade, format_args!("{:>+10.2}", stats.avg_trade))
        );
    }
}

fn print_best(heading: &str, rows: &[(String, TradeStats, usize)], colors: Palette) {
    if let Some((label, stats, days)) = rows
        .iter()
        .filter(|(_, stats, _)| stats.trades > 0)
        .max_by(|a, b| a.1.net_pnl.total_cmp(&b.1.net_pnl))
    {
        println!(
            "Best {heading} by net: {label} {} ({} trades, {days} days)",
            colors.pnl(stats.net_pnl, format_args!("{:+.2}", stats.net_pnl)),
            stats.trades
        );
    }
}
