//! Prepare replay bubbles away from the UI, advancing each session's trade cutoff once.

use std::sync::{mpsc, Arc};

use eframe::egui;

use crate::data::{cluster_bubbles, BubbleClustering, TimeAxis, TradeBubble, SEC_NS};
use crate::market_history::{at_x, TradeTape};

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct BubbleView {
    pub x: [f64; 2],
    pub pixels_per_unit: [f64; 2],
    pub clock: i64,
    pub follow: bool,
    pub auto_y: bool,
    pub min_trade: f64,
    pub clustering: BubbleClustering,
}

impl BubbleView {
    fn can_preview(self, old: Self) -> bool {
        self.clock >= old.clock
            && self.x[0] < old.x[1]
            && self.x[1] > old.x[0]
            && self.min_trade == old.min_trade
            && self.clustering == old.clustering
    }

    fn can_reuse(self, old: Self) -> bool {
        self.can_preview(old)
            && self.follow == old.follow
            && self.auto_y == old.auto_y
            && self.x[0] >= old.x[0]
            && self.x[1] >= old.x[1]
            && (self.follow || self.x == old.x)
            && close_scale(self.pixels_per_unit[0], old.pixels_per_unit[0], 1e-8)
            && (self.auto_y || close_scale(self.pixels_per_unit[1], old.pixels_per_unit[1], 1e-8))
    }

    fn can_advance(self, old: Self) -> bool {
        // Small automatic price-scale adjustments can arrive every frame.
        // Reuse the clusters until that scale has moved materially.
        let tolerance = if self.auto_y { 0.02 } else { 1e-8 };
        self.can_reuse(old)
            && close_scale(self.pixels_per_unit[1], old.pixels_per_unit[1], tolerance)
    }
}

fn close_scale(a: f64, b: f64, relative_tolerance: f64) -> bool {
    (a - b).abs() <= relative_tolerance * a.max(b)
}

pub(crate) struct BubbleGroup {
    pub trades: Vec<TradeBubble>,
    pub partial: Option<TradeBubble>,
    pub largest: f64,
}

impl BubbleGroup {
    pub fn new(trades: Vec<TradeBubble>, partial: Option<TradeBubble>, delta: bool) -> Self {
        let largest = trades
            .iter()
            .chain(&partial)
            .map(|bubble| bubble.clustering_volume(delta))
            .fold(0.0, f64::max);
        Self {
            trades,
            partial,
            largest,
        }
    }
}

struct SessionBubbles {
    session_start: i64,
    closed_until: i64,
    group: BubbleGroup,
}

pub(crate) struct BubbleFrame {
    view: BubbleView,
    column_ns: i64,
    sessions: Vec<SessionBubbles>,
}

impl BubbleFrame {
    fn build(axis: &TimeAxis, tape: &TradeTape, view: BubbleView, previous: Option<&Self>) -> Self {
        let previous = previous.filter(|old| view.can_advance(old.view));
        let column_ns = previous.map_or_else(
            || {
                let seconds = 1.0 / view.pixels_per_unit[0];
                if seconds >= 1.0 {
                    (seconds.ceil() as i64).saturating_mul(SEC_NS)
                } else {
                    (seconds * SEC_NS as f64).ceil().max(1.0) as i64
                }
            },
            |old| old.column_ns,
        );
        let pixels = previous.map_or(view.pixels_per_unit, |old| old.view.pixels_per_unit);
        let first = axis
            .segments
            .partition_point(|seg| seg.x0 + seg.len_secs() <= view.x[0]);
        let mut sessions = Vec::new();
        for segment in &axis.segments[first..] {
            if segment.x0 >= view.x[1] || segment.t0_ns > view.clock {
                break;
            }
            let aligned = |ts| segment.t0_ns + (ts - segment.t0_ns) / column_ns * column_ns;
            let left_x = segment.x0.max(view.x[0]);
            let right_x = view.x[1].min(segment.x0 + segment.len_secs());
            let from = aligned(at_x(segment, left_x));
            let until = at_x(segment, right_x).min(view.clock.saturating_add(1));
            if until <= from {
                continue;
            }
            let closed_until = if until == segment.t1_ns {
                until
            } else {
                aligned(until)
            };
            let prior = previous.and_then(|old| {
                old.sessions
                    .iter()
                    .find(|s| s.session_start == segment.t0_ns)
            });
            let mut settled: Vec<_> = prior
                .into_iter()
                .flat_map(|old| &old.group.trades)
                .copied()
                .filter(|bubble| bubble.x >= left_x)
                .collect();
            let start = prior.map_or(from, |old| from.max(old.closed_until));
            if start < closed_until {
                let new =
                    tape.bubble_columns(segment, start, closed_until, column_ns, view.min_trade);
                if !new.is_empty() {
                    settled.extend(new);
                    settled =
                        cluster_bubbles(settled.into_iter(), pixels, segment.x0, view.clustering);
                }
            }
            // Display the open column provisionally. It joins the persistent
            // clusters exactly once, after the clock has crossed its right edge.
            let partial = (closed_until < until)
                .then(|| {
                    tape.bubble_columns(segment, closed_until, until, column_ns, view.min_trade)
                        .into_iter()
                        .next()
                })
                .flatten();
            sessions.push(SessionBubbles {
                session_start: segment.t0_ns,
                closed_until,
                group: BubbleGroup::new(settled, partial, view.clustering.delta),
            });
        }
        Self {
            view: BubbleView {
                pixels_per_unit: pixels,
                ..view
            },
            column_ns,
            sessions,
        }
    }

    pub fn groups(&self) -> impl Iterator<Item = &BubbleGroup> {
        self.sessions.iter().map(|session| &session.group)
    }

    pub fn cutoff(&self, axis: &TimeAxis) -> i64 {
        self.view.clock.min(axis.to_ts(self.view.x[1]))
    }
}

struct Job {
    generation: u64,
    view: BubbleView,
    axis: TimeAxis,
    tape: TradeTape,
    previous: Option<Arc<BubbleFrame>>,
}

struct Worker {
    jobs: mpsc::Sender<Job>,
    results: mpsc::Receiver<(u64, Arc<BubbleFrame>)>,
    pending: bool,
}

impl Worker {
    fn spawn(ctx: &egui::Context) -> Self {
        let (jobs, receive_jobs) = mpsc::channel::<Job>();
        let (send_results, results) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for job in receive_jobs {
                let frame = Arc::new(BubbleFrame::build(
                    &job.axis,
                    &job.tape,
                    job.view,
                    job.previous.as_deref(),
                ));
                if send_results.send((job.generation, frame)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            jobs,
            results,
            pending: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct ReplayBubbles {
    generation: u64,
    worker: Option<Worker>,
    ready: Option<Arc<BubbleFrame>>,
}

impl ReplayBubbles {
    pub fn pending(&self) -> bool {
        self.worker.as_ref().is_some_and(|worker| worker.pending)
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.ready = None;
    }

    pub fn update(
        &mut self,
        ctx: &egui::Context,
        view: BubbleView,
        axis: &TimeAxis,
        tape: &TradeTape,
    ) -> Option<&BubbleFrame> {
        let worker = self.worker.get_or_insert_with(|| Worker::spawn(ctx));
        match worker.results.try_recv() {
            Ok((generation, frame)) => {
                worker.pending = false;
                if generation == self.generation
                    && view.can_preview(frame.view)
                    && (view.can_reuse(frame.view)
                        || self.ready.as_ref().is_none_or(|old| !view.can_preview(old.view)))
                {
                    self.ready = Some(frame);
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.worker = None;
                ctx.request_repaint();
                return self.ready.as_deref().filter(|frame| view.can_preview(frame.view));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // Continue drawing the previous clusters in chart coordinates. Only a frame
        // on the same grid can seed incremental aggregation at the new viewport.
        let ready = self
            .ready
            .as_ref()
            .filter(|frame| view.can_preview(frame.view));
        let previous = ready.filter(|frame| view.can_advance(frame.view));
        let up_to_date = previous.is_some_and(|frame| {
            let right = axis.to_ts(view.x[1]);
            frame.view.x == view.x && frame.view.clock.min(right) == view.clock.min(right)
        });
        if !worker.pending && !up_to_date {
            worker.pending = worker
                .jobs
                .send(Job {
                    generation: self.generation,
                    view,
                    axis: axis.clone(),
                    tape: tape.clone(),
                    previous: previous.cloned(),
                })
                .is_ok();
        }
        ready.map(Arc::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::{OrderFlowEvent, PreparedEventData, Side};

    #[test]
    fn replay_columns_are_committed_once_and_backward_seek_excludes_future_trades() {
        let trade = |ms: i64, size, aggressor| OrderFlowEvent::Trade {
            ts_ns: ms * 1_000_000,
            price: 100.0,
            size,
            aggressor,
        };
        let tape = TradeTape::new(
            Arc::new(
                PreparedEventData::from_events(vec![
                    OrderFlowEvent::Contract {
                        tick_size: 0.25,
                        multiplier: 50.0,
                    },
                    trade(10, 7.0, Side::Buy),
                    trade(110, 2.0, Side::Buy),
                    trade(160, 3.0, Side::Sell),
                    trade(220, 5.0, Side::Buy),
                ])
                .unwrap(),
            ),
            &[],
        );
        let axis = TimeAxis::build(vec![(0, SEC_NS - 1)], 0);
        let view = |ms: i64| BubbleView {
            x: [0.0, 1.0],
            pixels_per_unit: [10.0, 10.0],
            clock: ms * 1_000_000,
            follow: false,
            auto_y: false,
            min_trade: 0.0,
            clustering: BubbleClustering {
                strength: 0,
                target: 30.0,
                spherical: true,
                delta: false,
            },
        };
        let start = BubbleFrame::build(&axis, &tape, view(120), None);
        let group = start.groups().next().unwrap();
        assert_eq!(group.trades[0].total(), 7.0);
        assert_eq!(group.partial.unwrap().total(), 2.0);

        let next = BubbleFrame::build(&axis, &tape, view(165), Some(&start));
        let group = next.groups().next().unwrap();
        assert_eq!(group.trades.len(), 1);
        assert_eq!(group.partial.unwrap().total(), 5.0);

        let advanced = BubbleFrame::build(&axis, &tape, view(240), Some(&next));
        let group = advanced.groups().next().unwrap();
        assert_eq!(
            group
                .trades
                .iter()
                .map(|trade| trade.total())
                .collect::<Vec<_>>(),
            [7.0, 5.0]
        );
        assert_eq!((group.trades[1].buy, group.trades[1].sell), (2.0, 3.0));
        assert_eq!(group.partial.unwrap().total(), 5.0);

        let rewound = BubbleFrame::build(&axis, &tape, view(120), Some(&advanced));
        let group = rewound.groups().next().unwrap();
        assert_eq!(group.trades.len(), 1);
        assert_eq!(group.partial.unwrap().total(), 2.0);

        let mut zoomed_view = view(240);
        zoomed_view.pixels_per_unit[0] = 20.0;
        let zoomed = BubbleFrame::build(&axis, &tape, zoomed_view, Some(&advanced));
        assert_ne!(zoomed.column_ns, advanced.column_ns);
        assert_eq!(zoomed.groups().next().unwrap().trades.len(), 3);

        let ctx = egui::Context::default();
        let (jobs, requested) = mpsc::channel();
        let (completed, results) = mpsc::channel();
        let mut bubbles = ReplayBubbles {
            worker: Some(Worker { jobs, results, pending: false }),
            ready: Some(Arc::new(advanced)),
            ..Default::default()
        };
        let old_resolution = bubbles.ready.as_ref().unwrap().column_ns;
        let fallback = bubbles.update(&ctx, zoomed_view, &axis, &tape).unwrap();
        assert_eq!(fallback.column_ns, old_resolution);
        assert!(!fallback.groups().next().unwrap().trades.is_empty());
        let job = requested.try_recv().unwrap();
        assert!(job.previous.is_none(), "zoom must aggregate fresh clusters");

        let price_zoom = BubbleView {
            x: [0.05, 0.50],
            pixels_per_unit: [zoomed_view.pixels_per_unit[0], 30.0],
            ..zoomed_view
        };
        assert!(bubbles.update(&ctx, price_zoom, &axis, &tape).is_some());
        assert!(requested.try_recv().is_err(), "only one build may be in flight");
        completed.send((job.generation, Arc::new(zoomed))).unwrap();
        assert_eq!(bubbles.update(&ctx, price_zoom, &axis, &tape).unwrap().column_ns, old_resolution);
        let job = requested.try_recv().unwrap();
        assert_eq!(job.view.x, price_zoom.x);
        assert_eq!(job.view.pixels_per_unit, price_zoom.pixels_per_unit);
        let latest = Arc::new(BubbleFrame::build(&axis, &tape, job.view, job.previous.as_deref()));
        completed.send((job.generation, Arc::clone(&latest))).unwrap();
        assert!(std::ptr::eq(bubbles.update(&ctx, price_zoom, &axis, &tape).unwrap(), latest.as_ref()));
        assert!(bubbles.update(&ctx, view(120), &axis, &tape).is_none());
    }
}
