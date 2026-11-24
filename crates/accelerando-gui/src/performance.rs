use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::ChartMode;

const SAMPLE_COUNT: usize = 120;
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Viewer,
    Replay,
    ReplayBars,
    ReplayOverlays,
    Depth,
    Chart,
    Columns,
    Heatmap,
    Bubbles,
}

const PHASES: [(Phase, &str); 9] = [
    (Phase::Viewer, "Viewer UI"),
    (Phase::Replay, "Replay update"),
    (Phase::ReplayBars, "  Tick / footprint apply"),
    (Phase::ReplayOverlays, "  Strategy overlay sync"),
    (Phase::Depth, "Replay follow / depth"),
    (Phase::Chart, "Price chart"),
    (Phase::Columns, "  Quotes / Auto Y"),
    (Phase::Heatmap, "  Heatmap / depth edge"),
    (Phase::Bubbles, "  Trade bubbles"),
];

#[derive(Default)]
pub(crate) struct FrameTiming {
    phases: [Option<Duration>; PHASES.len()],
    interval: Option<Duration>,
    pub ticks_applied: usize,
    pub ticks_buffered: usize,
    pub ticks_loading: bool,
    pub depth_loading: bool,
    pub heat_pending: bool,
    pub heat_lag_ns: Option<i64>,
    pub bubbles_pending: bool,
    pub bubbles_lag_ns: Option<i64>,
}

impl FrameTiming {
    pub fn finish(&mut self, phase: Phase, start: Instant) {
        self.phases[phase as usize] = Some(start.elapsed());
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Playback {
    pub speed: f64,
    pub mode: ChartMode,
}

#[derive(Default)]
pub(crate) struct Performance {
    pub open: bool,
    playback: Option<Playback>,
    previous_start: Option<Instant>,
    samples: VecDeque<FrameTiming>,
    report: String,
    refreshed: Option<Instant>,
}

impl Performance {
    pub fn record(&mut self, start: Instant, mut sample: FrameTiming, playback: Playback) {
        if self.playback != Some(playback) {
            self.clear();
            self.playback = Some(playback);
        }
        sample.interval = self
            .previous_start
            .replace(start)
            .map(|last| start.duration_since(last));
        if self.samples.len() == SAMPLE_COUNT {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        if self
            .refreshed
            .is_none_or(|then| start.duration_since(then) >= REFRESH_INTERVAL)
        {
            self.refresh(start, playback);
        }
    }

    pub fn pause(&mut self) {
        if let (Some(start), Some(playback)) = (self.previous_start.take(), self.playback) {
            self.refresh(start, playback);
        }
    }

    pub fn close(&mut self) {
        if self.playback.take().is_some() {
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.previous_start = None;
        self.samples.clear();
        self.refreshed = None;
        self.report.clear();
    }

    fn refresh(&mut self, start: Instant, playback: Playback) {
        self.refreshed = Some(start);
        let mode = match playback.mode {
            ChartMode::Bar => "Bar",
            ChartMode::Heatmap => "Heatmap",
        };
        let count = self.samples.len();
        let mut report = format!(
            "{mode}  |  {}×  |  {count} sampled frames\n",
            playback.speed
        );
        let intervals = self.samples.iter().filter_map(|sample| sample.interval);
        let total: Duration = intervals.clone().sum();
        if !total.is_zero() {
            let updates_per_sec = intervals.count() as f64 / total.as_secs_f64();
            let slow = self
                .samples
                .iter()
                .filter_map(|sample| sample.interval)
                .filter(|interval| *interval > Duration::from_millis(33))
                .count();
            let _ = writeln!(
                report,
                "{updates_per_sec:.1} updates/s  |  {slow} gaps > 33 ms"
            );
        }
        let _ = writeln!(
            report,
            "\n{:<25}{:>8}{:>8}{:>8}",
            "Milliseconds", "Mean", "P95", "Max"
        );
        let mut values = Vec::with_capacity(count);
        let rows = std::iter::once((None, "Update interval")).chain(
            PHASES
                .into_iter()
                .map(|(phase, label)| (Some(phase), label)),
        );
        for (phase, label) in rows {
            values.clear();
            values.extend(
                self.samples
                    .iter()
                    .filter_map(|sample| match phase {
                        Some(phase) => sample.phases[phase as usize],
                        None => sample.interval,
                    })
                    .map(|time| time.as_secs_f64() * 1000.0),
            );
            values.sort_unstable_by(f64::total_cmp);
            if let Some(max) = values.last() {
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                let p95 = values[(values.len() * 95).div_ceil(100) - 1];
                let _ = writeln!(report, "{label:<25}{mean:>8.2}{p95:>8.2}{max:>8.2}");
            }
        }
        if let Some(last) = self.samples.back() {
            let tick_status = if last.ticks_loading {
                " (fetch pending)"
            } else {
                ""
            };
            let depth_status = if last.depth_loading {
                "pending"
            } else {
                "idle"
            };
            let _ = writeln!(
                report,
                "\nLatest: {} ticks applied, {} buffered{tick_status}",
                last.ticks_applied, last.ticks_buffered
            );
            let _ = writeln!(report, "Depth window load: {depth_status}");
            for (label, lag_ns, pending) in [
                ("Resting heatmap", last.heat_lag_ns, last.heat_pending),
                ("Trade bubbles", last.bubbles_lag_ns, last.bubbles_pending),
            ] {
                let task = if pending {
                    "worker pending"
                } else {
                    "worker idle"
                };
                if let Some(lag_ns) = lag_ns {
                    let lag = lag_ns.max(0) as f64 / 1e9;
                    let _ = writeln!(report, "{label}: {lag:.3} s behind replay, {task}");
                } else if pending {
                    let _ = writeln!(report, "{label}: waiting for data, {task}");
                }
            }
        }
        self.report = report;
    }

    pub fn show(&mut self, ctx: &egui::Context, playing: bool) {
        let mut reset = false;
        egui::Window::new("Performance")
            .open(&mut self.open)
            .default_pos(egui::pos2(24.0, 84.0))
            .resizable(false)
            .show(ctx, |ui| {
                if self.report.is_empty() {
                    ui.label("Play a replay to collect frame timings.");
                } else {
                    ui.label(if playing { "Recording playback" } else { "Last playback capture" });
                    ui.monospace(self.report.trim_end());
                    ui.weak("Stage timings cover viewer work on the UI thread.\nLayer lag is measured in replay seconds.");
                    ui.horizontal(|ui| {
                        if ui.button("Copy report").clicked() {
                            ctx.copy_text(self.report.clone());
                        }
                        reset = ui.button("Reset").clicked();
                    });
                }
            });
        if reset {
            self.clear();
        }
        if !self.open {
            self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_frame_gaps_and_keeps_pause_time_out_of_cadence() {
        let mut performance = Performance::default();
        let start = Instant::now();
        let playback = Playback {
            speed: 10.0,
            mode: ChartMode::Heatmap,
        };
        let record = |performance: &mut Performance, elapsed_ms| {
            let sample = FrameTiming {
                heat_lag_ns: Some(500_000_000),
                heat_pending: true,
                ..Default::default()
            };
            performance.record(start + Duration::from_millis(elapsed_ms), sample, playback);
        };
        record(&mut performance, 0);
        record(&mut performance, 10);
        record(&mut performance, 50);
        performance.pause();
        assert!(performance
            .report
            .contains("40.0 updates/s  |  1 gaps > 33 ms"));
        assert!(performance
            .report
            .contains("0.500 s behind replay, worker pending"));

        record(&mut performance, 1000);
        assert!(performance.report.contains("40.0 updates/s"));
        assert!(performance.samples.back().unwrap().interval.is_none());
        for index in 1..=SAMPLE_COUNT as u64 {
            record(&mut performance, 1000 + 10 * index);
        }
        performance.pause();
        assert_eq!(performance.samples.len(), SAMPLE_COUNT);
        assert!(performance
            .report
            .contains("100.0 updates/s  |  0 gaps > 33 ms"));
    }
}
