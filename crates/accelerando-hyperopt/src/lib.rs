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
        self.add_spec_excluding(namespace, spec, &[]);
    }

    /// Like [`SearchSpace::add_spec`], but skips entries named in `exclude`. Callers use this to
    /// pin parameters to explicit values: a pinned dimension is removed from the search entirely
    /// instead of being enumerated, and the pinned value is merged into every candidate afterward.
    pub fn add_spec_excluding(&mut self, namespace: &str, spec: &ParamSpec, exclude: &[&str]) {
        for e in &spec.entries {
            if matches!(e.range, ParamRange::Fixed) || exclude.contains(&e.name.as_str()) {
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
    let candidates = generate_candidates(space, algo, evals, seed);

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

/// Generate concrete candidates without evaluating them.
///
/// Framework facade APIs use this when an embedding application wants parameter sets and owns the
/// backtest scheduling itself. Keeping generation here prevents consumers from reimplementing grid
/// enumeration or abusing a dummy evaluator merely to obtain candidates.
pub fn generate_candidates(
    space: &SearchSpace,
    algo: Algo,
    evals: usize,
    seed: u64,
) -> Vec<Params> {
    let evals = evals.max(1);
    match algo {
        Algo::Random => {
            let mut rng = XorShift::new(seed);
            (0..evals)
                .map(|_| space.sample(&mut rng))
                .collect::<Vec<_>>()
        }
        Algo::Grid => grid(space, evals),
    }
}

/// Total number of points in the full cartesian grid (saturating).
pub fn grid_size(space: &SearchSpace) -> u128 {
    space
        .dims
        .iter()
        .map(|d| axis_values(&d.range).len().max(1) as u128)
        .fold(1u128, |acc, n| acc.saturating_mul(n))
}

/// Enumerate the cartesian grid over the dimensions.
///
/// When the full grid is larger than `cap`, `cap` points are taken at evenly spaced positions of
/// the full enumeration (mixed-radix decoding), so *every* dimension still varies. (Plain
/// truncation would only ever vary the first one or two dimensions and pin the rest at their
/// minimum.)
fn grid(space: &SearchSpace, cap: usize) -> Vec<Params> {
    let axes: Vec<(String, Vec<ParamValue>)> = space
        .dims
        .iter()
        .map(|d| (d.key.clone(), axis_values(&d.range)))
        .filter(|(_, values)| !values.is_empty())
        .collect();
    let total = axes
        .iter()
        .fold(1u128, |acc, (_, v)| acc.saturating_mul(v.len() as u128));
    let cap = cap.max(1) as u128;
    let count = total.min(cap);
    let decode = |mut index: u128| {
        let mut p = Params::default();
        // Last axis varies fastest, matching the nested-loop order of a full enumeration.
        for (key, values) in axes.iter().rev() {
            let n = values.len() as u128;
            p.0.insert(key.clone(), values[(index % n) as usize].clone());
            index /= n;
        }
        p
    };
    (0..count)
        .map(|k| {
            let index = if total <= cap {
                k
            } else {
                // Spread across the whole grid; include both corners when count > 1.
                if count == 1 {
                    0
                } else {
                    k.saturating_mul(total - 1) / (count - 1)
                }
            };
            decode(index)
        })
        .collect()
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

#[cfg(test)]
mod grid_sampling_tests {
    use super::*;
    use accelerando_core::ParamSpec;

    #[test]
    fn capped_grid_varies_every_dimension_and_full_grid_is_unchanged() {
        let spec = ParamSpec::new()
            .int("a", 0, 0, 99, 1)
            .int("b", 0, 0, 9, 1)
            .int("c", 0, 0, 9, 1);
        let mut space = SearchSpace::new();
        space.add_spec_excluding("s", &spec, &[]);
        assert_eq!(grid_size(&space), 10_000);
        let capped = generate_candidates(&space, Algo::Grid, 50, 0);
        assert_eq!(capped.len(), 50);
        for key in ["s.a", "s.b", "s.c"] {
            let mut distinct: Vec<i64> = capped
                .iter()
                .map(|p| p.get(key).and_then(ParamValue::as_i64).unwrap())
                .collect();
            distinct.sort_unstable();
            distinct.dedup();
            assert!(distinct.len() > 3, "{key} barely varies: {distinct:?}");
        }
        let small = ParamSpec::new().int("x", 0, 0, 2, 1).int("y", 0, 0, 1, 1);
        let mut space = SearchSpace::new();
        space.add_spec_excluding("s", &small, &[]);
        let full = generate_candidates(&space, Algo::Grid, 100, 0);
        let pairs: Vec<(i64, i64)> = full
            .iter()
            .map(|p| (p.int("s.x", -1), p.int("s.y", -1)))
            .collect();
        assert_eq!(pairs, vec![(0, 0), (0, 1), (1, 0), (1, 1), (2, 0), (2, 1)]);
    }
}
