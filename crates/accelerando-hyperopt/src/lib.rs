//! Parallel parameter search — "the world has been accelerated".
//!
//! The hyperopt layer is deliberately decoupled from the concrete adapters: it knows only a
//! [`SearchSpace`] (namespaced parameter keys + ranges) and an *evaluator* closure that builds and
//! runs a pipeline for one candidate `Params` and returns a scalar objective to **maximize**. The
//! embedding application supplies that closure, so hyperopt depends only on
//! `accelerando-core` (+ `rayon`).
//!
//! Today the [`BatchEvaluator`] seam runs candidates across CPU cores with `rayon`. A future
//! `wgpu` implementation can satisfy the same seam for GPU batch evaluation (Phase 3).

use accelerando_core::{ParamRange, ParamSpec, ParamValue, Params};
use rayon::prelude::*;

mod rng;
use rng::XorShift;

/// One tunable dimension of the search.
#[derive(Clone, Debug)]
pub struct Dim {
    pub key: String,
    pub range: ParamRange,
}

/// The full search space: only the tunable (non-`Fixed`) parameters, with namespaced keys.
#[derive(Clone, Debug, Default)]
pub struct SearchSpace {
    pub dims: Vec<Dim>,
}

impl SearchSpace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add every tunable entry of `spec`, prefixing each key with `namespace.`.
    pub fn add_spec(&mut self, namespace: &str, spec: &ParamSpec) {
        for e in &spec.entries {
            if matches!(e.range, ParamRange::Fixed) {
                continue;
            }
            self.dims.push(Dim {
                key: format!("{namespace}.{}", e.name),
                range: e.range.clone(),
            });
        }
    }

    fn sample(&self, rng: &mut XorShift) -> Params {
        let mut p = Params::default();
        for d in &self.dims {
            let v = match &d.range {
                ParamRange::Int { min, max, step } => {
                    let step = (*step).max(1);
                    let steps = ((max - min) / step).max(0);
                    let k = rng.below((steps + 1) as u64) as i64;
                    ParamValue::Int(min + k * step)
                }
                ParamRange::Float { min, max, step } => match step {
                    Some(s) if *s > 0.0 => {
                        let steps = (((max - min) / s).floor() as i64).max(0);
                        let k = rng.below((steps + 1) as u64) as f64;
                        ParamValue::Float(min + k * s)
                    }
                    _ => ParamValue::Float(min + rng.unit() * (max - min)),
                },
                ParamRange::Choice(opts) => {
                    let i = rng.below(opts.len().max(1) as u64) as usize;
                    ParamValue::Str(opts[i % opts.len().max(1)].clone())
                }
                ParamRange::Fixed => continue,
            };
            p.0.insert(d.key.clone(), v);
        }
        p
    }
}

/// Which search algorithm to use.
#[derive(Clone, Copy, Debug)]
pub enum Algo {
    Random,
    Grid,
}

impl Algo {
    pub fn parse(s: &str) -> Option<Algo> {
        match s.to_lowercase().as_str() {
            "random" | "rand" => Some(Algo::Random),
            "grid" => Some(Algo::Grid),
            _ => None,
        }
    }
}

/// One evaluated candidate.
#[derive(Clone, Debug)]
pub struct Trial {
    pub params: Params,
    pub score: f64,
}

/// The outcome of a search.
#[derive(Clone, Debug)]
pub struct SearchReport {
    pub best: Trial,
    pub trials: Vec<Trial>,
}

/// A batch of candidates evaluated in parallel. Implemented for CPU here; reserved for GPU later.
pub trait BatchEvaluator: Sync {
    fn evaluate(&self, candidates: &[Params]) -> Vec<f64>;
}

/// CPU evaluator: runs the user closure across rayon's thread pool.
pub struct CpuEvaluator<F: Fn(&Params) -> f64 + Sync> {
    pub func: F,
}

impl<F: Fn(&Params) -> f64 + Sync> BatchEvaluator for CpuEvaluator<F> {
    fn evaluate(&self, candidates: &[Params]) -> Vec<f64> {
        candidates.par_iter().map(|p| (self.func)(p)).collect()
    }
}

/// Run a search. `evals` caps the number of candidates (for grid, the grid is truncated to it).
pub fn search<E: BatchEvaluator>(
    space: &SearchSpace,
    algo: Algo,
    evals: usize,
    seed: u64,
    evaluator: &E,
) -> SearchReport {
    let candidates = match algo {
        Algo::Random => {
            let mut rng = XorShift::new(seed);
            (0..evals)
                .map(|_| space.sample(&mut rng))
                .collect::<Vec<_>>()
        }
        Algo::Grid => grid(space, evals),
    };

    let scores = evaluator.evaluate(&candidates);
    let trials: Vec<Trial> = candidates
        .into_iter()
        .zip(scores)
        .map(|(params, score)| Trial { params, score })
        .collect();

    let best = trials
        .iter()
        .filter(|t| t.score.is_finite())
        .cloned()
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .unwrap_or_else(|| trials.first().cloned().expect("at least one trial"));

    SearchReport { best, trials }
}

/// Enumerate a (truncated) cartesian grid over the dimensions.
fn grid(space: &SearchSpace, cap: usize) -> Vec<Params> {
    let axes: Vec<(String, Vec<ParamValue>)> = space
        .dims
        .iter()
        .map(|d| (d.key.clone(), axis_values(&d.range)))
        .collect();

    let mut out = vec![Params::default()];
    for (key, values) in &axes {
        let mut next = Vec::new();
        for base in &out {
            for v in values {
                let mut p = base.clone();
                p.0.insert(key.clone(), v.clone());
                next.push(p);
                if next.len() >= cap {
                    break;
                }
            }
            if next.len() >= cap {
                break;
            }
        }
        out = next;
        if out.len() >= cap {
            out.truncate(cap);
        }
    }
    out
}

fn axis_values(range: &ParamRange) -> Vec<ParamValue> {
    match range {
        ParamRange::Int { min, max, step } => {
            let step = (*step).max(1);
            let mut v = Vec::new();
            let mut x = *min;
            while x <= *max {
                v.push(ParamValue::Int(x));
                x += step;
            }
            v
        }
        ParamRange::Float { min, max, step } => {
            let s = step.unwrap_or((max - min) / 4.0).max(f64::EPSILON);
            let mut v = Vec::new();
            let mut x = *min;
            while x <= *max + 1e-9 {
                v.push(ParamValue::Float(x));
                x += s;
            }
            v
        }
        ParamRange::Choice(opts) => opts.iter().cloned().map(ParamValue::Str).collect(),
        ParamRange::Fixed => Vec::new(),
    }
}
