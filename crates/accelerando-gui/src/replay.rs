//! Market replay: pick a bar, hide everything after it and play the raw trades back tick by tick
//! at an adjustable speed. The bar being replayed is rebuilt from the ticks; bars that are
//! complete are shown exactly as the aggregator produced them.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};

use accelerando_core::{Footprint, Level, OrderFlowEvent, Side};
use eframe::egui;

use crate::chart::ChartState;
use crate::Model;

/// One trade of the replay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    pub ts_ns: i64,
    pub price: f64,
    pub size: f64,
    /// Aggressor: `1` buy, `-1` sell, `0` unknown.
    pub side: i8,
}

/// Host-supplied tick reader: `(from_ts_ns, max)` returns the next trades with
/// `ts_ns >= from_ts_ns` in time order - about `max` of them, but never splitting trades that
/// share a timestamp. An empty result means the data ends there. Runs on a background thread;
/// [`ticks_from`] implements it over raw events.
pub type TickSource = Arc<dyn Fn(i64, usize) -> Result<Vec<Tick>, String> + Send + Sync>;

/// The trades of `events` (time-ordered, as the framework prepares them) from `from_ts_ns` on:
/// `max` of them, plus any further ones with the same timestamp as the last.
pub fn ticks_from(events: &[OrderFlowEvent], from_ts_ns: i64, max: usize) -> Vec<Tick> {
    let ts = |ev: &OrderFlowEvent| match *ev {
        OrderFlowEvent::Contract { .. } => i64::MIN,
        OrderFlowEvent::Trade { ts_ns, .. }
        | OrderFlowEvent::AddLimit { ts_ns, .. }
        | OrderFlowEvent::ReduceLimit { ts_ns, .. }
        | OrderFlowEvent::SetLevel { ts_ns, .. } => ts_ns,
    };
    let start = events.partition_point(|ev| ts(ev) < from_ts_ns);
    let mut out: Vec<Tick> = Vec::with_capacity(max.min(events.len() - start));
    for ev in &events[start..] {
        let OrderFlowEvent::Trade { ts_ns, price, size, aggressor } = *ev else { continue };
        if out.len() >= max && out.last().is_some_and(|t| t.ts_ns != ts_ns) {
            break;
        }
        let side = match aggressor {
            Side::Buy => 1,
            Side::Sell => -1,
            Side::Unknown => 0,
        };
        out.push(Tick { ts_ns, price, size, side });
    }
    out
}

/// The bars visible during a replay: the complete ones, then the one being built.
pub(crate) struct ReplayBars {
    /// `full[..done]` verbatim, plus the bar in progress when `partial`.
    pub bars: Vec<Footprint>,
    done: usize,
    partial: bool,
    /// Ladder of the bar in progress, by price in ticks.
    ladder: BTreeMap<i64, (f64, f64)>,
    tick_size: f64,
    /// Replay time: nothing later than this is shown.
    pub clock_ns: i64,
}

impl ReplayBars {
    /// Start in front of `full[start]`.
    pub fn new(full: &[Footprint], start: usize, tick_size: f64) -> Self {
        let start = start.min(full.len());
        Self {
            bars: full[..start].to_vec(),
            done: start,
            partial: false,
            ladder: BTreeMap::new(),
            tick_size: tick_size.abs().max(1e-9),
            clock_ns: full.get(start).map_or(i64::MAX, |f| f.ts_first_ns - 1),
        }
    }

    /// Every bar has been replayed.
    pub fn finished(&self, full: &[Footprint]) -> bool {
        self.done >= full.len()
    }

    /// End of the bar in progress (or of the next one), for "step one bar".
    pub fn bar_end_ns(&self, full: &[Footprint]) -> Option<i64> {
        full.get(self.done).map(|f| f.ts_last_ns)
    }

    fn finalize(&mut self, full: &[Footprint]) {
        let bar = full[self.done].clone();
        if self.partial {
            *self.bars.last_mut().expect("partial bar") = bar;
        } else {
            self.bars.push(bar);
        }
        self.done += 1;
        self.partial = false;
        self.ladder.clear();
    }

    /// Feed one trade into the bar it belongs to.
    pub fn apply(&mut self, t: &Tick, full: &[Footprint]) {
        loop {
            let Some(bar) = full.get(self.done) else { return };
            // The bar is over when the trade is later than its last one - or, for trades sharing
            // that timestamp, once the bar already holds all of its trades.
            let have = if self.partial { self.bars.last().map_or(0, |b| b.trades) } else { 0 };
            let next_starts = full.get(self.done + 1).is_some_and(|n| t.ts_ns >= n.ts_first_ns);
            if t.ts_ns > bar.ts_last_ns || (have >= bar.trades && next_starts) {
                self.finalize(full);
            } else {
                break;
            }
        }
        // Trades of bars that were completed by the clock (stepping a whole bar) are skipped.
        if !self.partial && self.done > 0 && t.ts_ns < full[self.done - 1].ts_last_ns {
            return;
        }
        let (buy, sell) = match t.side {
            1 => (t.size, 0.0),
            -1 => (0.0, t.size),
            _ => (0.0, 0.0),
        };
        if !self.partial {
            self.partial = true;
            self.bars.push(Footprint {
                ts_first_ns: t.ts_ns,
                ts_last_ns: t.ts_ns,
                open: t.price,
                high: t.price,
                low: t.price,
                close: t.price,
                volume: 0.0,
                trades: 0,
                delta: 0.0,
                poc: t.price,
                ladder: Vec::new(),
                values: Default::default(),
                tags: Default::default(),
                plots: Vec::new(),
            });
        }
        let level = self.ladder.entry((t.price / self.tick_size).round() as i64).or_insert((0.0, 0.0));
        level.0 += buy;
        level.1 += sell;
        let bar = self.bars.last_mut().expect("partial bar");
        bar.ts_last_ns = t.ts_ns;
        bar.high = bar.high.max(t.price);
        bar.low = bar.low.min(t.price);
        bar.close = t.price;
        bar.volume += t.size;
        bar.trades += 1;
        bar.delta += buy - sell;
        bar.ladder.clear();
        let mut best = f64::NEG_INFINITY;
        for (&k, &(buy_vol, sell_vol)) in &self.ladder {
            let price = k as f64 * self.tick_size;
            if buy_vol + sell_vol > best {
                best = buy_vol + sell_vol;
                bar.poc = price;
            }
            bar.ladder.push(Level { price, buy_vol, sell_vol });
        }
    }

    /// Move the clock; bars that ended before it are complete.
    pub fn set_clock(&mut self, clock_ns: i64, full: &[Footprint]) {
        self.clock_ns = self.clock_ns.max(clock_ns);
        while full.get(self.done).is_some_and(|bar| self.clock_ns > bar.ts_last_ns) {
            self.finalize(full);
        }
    }
}

/// Ticks fetched per request.
const CHUNK: usize = 200_000;
/// Waits longer than this (in real seconds) are skipped: overnight, weekends, dead minutes.
const MAX_WAIT_SECS: f64 = 1.5;
pub(crate) const SPEED_RANGE: std::ops::RangeInclusive<f64> = 0.1..=5000.0;

/// Playback state and controls.
pub(crate) struct Replay {
    /// The next click on the chart picks the bar to start from.
    pub picking: bool,
    pub playing: bool,
    /// Replay seconds per real second.
    pub speed: f64,
    /// Keep the replay position in view.
    pub follow: bool,
    pub error: Option<String>,
    ticks: Vec<Tick>,
    next: usize,
    /// Where the next fetch starts; `None` once the source ran dry.
    fetch_from: Option<i64>,
    pending: Option<mpsc::Receiver<Result<Vec<Tick>, String>>>,
}

impl Default for Replay {
    fn default() -> Self {
        Self {
            picking: false,
            playing: false,
            speed: 10.0,
            follow: true,
            error: None,
            ticks: Vec::new(),
            next: 0,
            fetch_from: None,
            pending: None,
        }
    }
}

impl Replay {
    /// Start replaying in front of bar `index` of the displayed bars.
    pub fn start(&mut self, model: &mut Model, index: usize) {
        let Some(first) = model.full_bars().get(index).map(|f| f.ts_first_ns) else { return };
        model.start_replay(index);
        self.picking = false;
        self.playing = true;
        self.error = None;
        self.ticks.clear();
        self.next = 0;
        self.fetch_from = Some(first);
        self.pending = None;
    }

    pub fn stop(&mut self, model: &mut Model) {
        model.stop_replay();
        self.picking = false;
        self.playing = false;
        self.ticks.clear();
        self.next = 0;
        self.fetch_from = None;
        self.pending = None;
    }

    fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Fetch more ticks when the buffer runs low.
    fn feed(&mut self, source: &TickSource, ctx: &egui::Context) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Ok(ticks)) => {
                    self.pending = None;
                    self.fetch_from = ticks.last().map(|t| t.ts_ns + 1);
                    self.ticks.extend(ticks);
                }
                Ok(Err(error)) => {
                    self.pending = None;
                    self.fetch_from = None;
                    self.playing = false;
                    self.error = Some(error);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.fetch_from = None;
                    self.error = Some("tick reader stopped".to_string());
                }
            }
        }
        if self.next > CHUNK {
            self.ticks.drain(..self.next);
            self.next = 0;
        }
        let low = self.ticks.len() - self.next < CHUNK / 4;
        if let (true, None, Some(from)) = (low, &self.pending, self.fetch_from) {
            let (tx, rx) = mpsc::channel();
            let (source, ctx) = (Arc::clone(source), ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(source(from, CHUNK));
                ctx.request_repaint();
            });
            self.pending = Some(rx);
        }
    }

    /// Apply buffered ticks up to `clock_ns` and move the model's clock there.
    fn advance_to(&mut self, model: &mut Model, mut clock_ns: i64) {
        // Never run ahead of what has been loaded.
        if let Some(from) = self.fetch_from {
            clock_ns = clock_ns.min(from - 1);
        }
        let end = self.next + self.ticks[self.next..].partition_point(|t| t.ts_ns <= clock_ns);
        model.replay_apply(&self.ticks[self.next..end], clock_ns);
        self.next = end;
    }

    /// Advance exactly one trade.
    pub fn step_tick(&mut self, model: &mut Model) {
        self.playing = false;
        let Some(tick) = self.ticks.get(self.next).copied() else { return };
        model.replay_apply(&[tick], tick.ts_ns);
        self.next += 1;
    }

    /// Advance to the end of the bar in progress.
    pub fn step_bar(&mut self, model: &mut Model) {
        self.playing = false;
        if let Some(end) = model.replay_bar_end() {
            self.advance_to(model, end + 1);
        }
    }

    /// Once per frame while a replay is active.
    pub fn update(
        &mut self,
        model: &mut Model,
        chart: &mut ChartState,
        source: &TickSource,
        ctx: &egui::Context,
    ) {
        let Some(clock) = model.replay_clock() else { return };
        self.feed(source, ctx);
        let before = model.replay_cut_x();
        if self.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1) as f64;
            let mut target = clock + (dt * self.speed * 1e9) as i64;
            if let Some(next) = self.ticks.get(self.next) {
                // Nothing trades for a while: jump to the next trade.
                if (next.ts_ns - target) as f64 > MAX_WAIT_SECS * self.speed * 1e9 {
                    target = next.ts_ns;
                }
            }
            self.advance_to(model, target);
            if model.replay_finished() {
                self.playing = false;
            }
            ctx.request_repaint();
        }
        if self.loading() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        // Follow: keep the replay position inside the right part of the view.
        let after = model.replay_cut_x();
        if let (true, Some(x)) = (self.follow && after != before, after) {
            let span = chart.x1 - chart.x0;
            if x > chart.x1 - span * 0.08 || x < chart.x0 {
                chart.show_range(x - span * 0.8, x + span * 0.2);
            }
        }
    }

    /// The transport controls shown in the toolbar while a replay is active.
    pub fn controls(&mut self, ui: &mut egui::Ui, model: &mut Model) {
        let finished = model.replay_finished();
        let play = if self.playing { "⏸ Pause" } else { "▶ Play" };
        if ui.add_enabled(!finished, egui::Button::new(play)).on_hover_text("Space").clicked() {
            self.playing = !self.playing;
        }
        if ui.add_enabled(!finished, egui::Button::new("Tick ▸")).on_hover_text("Next trade (→)").clicked() {
            self.step_tick(model);
        }
        if ui.add_enabled(!finished, egui::Button::new("Bar ▸▸")).on_hover_text("Finish this bar (Shift + →)").clicked() {
            self.step_bar(model);
        }
        ui.label("Speed");
        for (label, factor) in [("−", 0.5), ("+", 2.0)] {
            if ui.small_button(label).on_hover_text("Halve / double (↓ / ↑)").clicked() {
                self.set_speed(self.speed * factor);
            }
        }
        ui.add(
            egui::Slider::new(&mut self.speed, SPEED_RANGE)
                .logarithmic(true)
                .suffix("x")
                .max_decimals(1),
        )
        .on_hover_text("Replay seconds per real second. Quiet stretches are skipped.");
        ui.checkbox(&mut self.follow, "Follow");
        if ui.button("⏮ Pick bar").on_hover_text("Click another bar to restart from it").clicked() {
            self.stop(model);
            self.picking = true;
            return;
        }
        if ui.button("✖ Exit replay").clicked() {
            self.stop(model);
            return;
        }
        if self.loading() && self.next >= self.ticks.len() {
            ui.spinner();
            ui.label("loading ticks ...");
        } else if finished {
            ui.weak("end of data");
        }
        if let Some(error) = &self.error {
            ui.colored_label(crate::chart::LOSS, error);
        }
    }

    fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(*SPEED_RANGE.start(), *SPEED_RANGE.end());
    }

    /// Keyboard shortcuts while a replay is active and no text field has the focus.
    pub fn shortcuts(&mut self, ctx: &egui::Context, model: &mut Model) {
        if ctx.memory(|m| m.focused().is_some()) || model.replay_clock().is_none() {
            return;
        }
        let (space, right, shift, up, down) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.shift,
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        if space {
            self.playing = !self.playing;
        }
        match (right, shift) {
            (true, true) => self.step_bar(model),
            (true, false) => self.step_tick(model),
            _ => {}
        }
        if up {
            self.set_speed(self.speed * 2.0);
        }
        if down {
            self.set_speed(self.speed * 0.5);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts0: i64, ts1: i64, trades: u32) -> Footprint {
        Footprint {
            ts_first_ns: ts0,
            ts_last_ns: ts1,
            open: 1.0,
            high: 9.0,
            low: 1.0,
            close: 2.0,
            volume: 100.0,
            trades,
            delta: 0.0,
            poc: 1.0,
            ladder: Vec::new(),
            values: Default::default(),
            tags: Default::default(),
            plots: Vec::new(),
        }
    }

    fn tick(ts_ns: i64, price: f64, size: f64, side: i8) -> Tick {
        Tick { ts_ns, price, size, side }
    }

    #[test]
    fn ticks_build_the_bar_in_progress_and_complete_bars_are_verbatim() {
        let full = vec![bar(0, 9, 2), bar(10, 19, 3), bar(20, 29, 1)];
        let mut r = ReplayBars::new(&full, 1, 0.25);
        assert_eq!((r.bars.len(), r.clock_ns), (1, 9));

        r.apply(&tick(10, 100.0, 2.0, 1), &full);
        r.apply(&tick(12, 100.5, 3.0, -1), &full);
        let b = r.bars.last().unwrap();
        assert_eq!(r.bars.len(), 2);
        assert_eq!((b.open, b.high, b.low, b.close), (100.0, 100.5, 100.0, 100.5));
        assert_eq!((b.volume, b.delta, b.trades, b.poc), (5.0, -1.0, 2, 100.5));
        assert_eq!(b.ladder.len(), 2);

        // A trade of the next bar completes this one with the aggregator's own bar.
        r.apply(&tick(20, 101.0, 1.0, 1), &full);
        assert_eq!(r.bars.len(), 3);
        assert_eq!(r.bars[1].volume, 100.0);
        assert_eq!(r.bars[2].volume, 1.0);

        // The clock alone completes a bar too.
        r.set_clock(30, &full);
        assert!(r.finished(&full));
        assert_eq!(r.bars[2].volume, 100.0);
    }

    #[test]
    fn same_timestamp_trades_split_by_trade_count() {
        // Volume-style bars: the first ends and the second starts at ts 5.
        let full = vec![bar(0, 5, 2), bar(5, 8, 2)];
        let mut r = ReplayBars::new(&full, 0, 0.25);
        for t in [tick(0, 1.0, 1.0, 1), tick(5, 1.0, 1.0, 1), tick(5, 2.0, 1.0, 1)] {
            r.apply(&t, &full);
        }
        assert_eq!(r.bars.len(), 2);
        assert_eq!((r.bars[1].open, r.bars[1].trades), (2.0, 1));
    }

    #[test]
    fn ticks_from_reads_chunks_without_splitting_a_timestamp() {
        let trade = |ts_ns, price| OrderFlowEvent::Trade { ts_ns, price, size: 1.0, aggressor: Side::Buy };
        let events = vec![
            OrderFlowEvent::Contract { tick_size: 0.25, multiplier: 50.0 },
            trade(1, 1.0),
            trade(2, 2.0),
            trade(2, 3.0),
            trade(3, 4.0),
        ];
        let first = ticks_from(&events, 0, 2);
        assert_eq!(first.iter().map(|t| t.price).collect::<Vec<_>>(), vec![1.0, 2.0, 3.0]);
        let rest = ticks_from(&events, first.last().unwrap().ts_ns + 1, 2);
        assert_eq!(rest.iter().map(|t| t.price).collect::<Vec<_>>(), vec![4.0]);
        assert!(ticks_from(&events, 4, 2).is_empty());
    }
}
