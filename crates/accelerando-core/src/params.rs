//! Data-driven parameters shared by every pluggable stage.
//!
//! A component advertises its tunable knobs through [`ParamSpec`] (used to build the hyperopt
//! search space and to supply defaults) and is constructed from a [`Params`] map.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A concrete parameter value. `Str` carries non-tunable strings (e.g. file paths) and the
/// selected option of a [`ParamRange::Choice`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    Int(i64),
    Float(f64),
    Str(String),
}

impl ParamValue {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ParamValue::Int(v) => Some(*v),
            ParamValue::Float(v) => Some(*v as i64),
            ParamValue::Str(_) => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ParamValue::Int(v) => Some(*v as f64),
            ParamValue::Float(v) => Some(*v),
            ParamValue::Str(_) => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            ParamValue::Str(v) => Some(v),
            _ => None,
        }
    }
}

/// The tunable domain of a single parameter, consumed by the hyperopt samplers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ParamRange {
    Int {
        min: i64,
        max: i64,
        step: i64,
    },
    Float {
        min: f64,
        max: f64,
        step: Option<f64>,
    },
    Choice(Vec<String>),
    /// A fixed value that is never searched (paths, flags).
    Fixed,
}

/// One declared parameter: its name, search domain and default value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParamEntry {
    pub name: String,
    pub range: ParamRange,
    pub default: ParamValue,
}

/// The full set of parameters a component declares.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParamSpec {
    pub entries: Vec<ParamEntry>,
}

impl ParamSpec {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn int(mut self, name: &str, default: i64, min: i64, max: i64, step: i64) -> Self {
        self.entries.push(ParamEntry {
            name: name.to_string(),
            range: ParamRange::Int { min, max, step },
            default: ParamValue::Int(default),
        });
        self
    }

    pub fn float(mut self, name: &str, default: f64, min: f64, max: f64) -> Self {
        self.entries.push(ParamEntry {
            name: name.to_string(),
            range: ParamRange::Float {
                min,
                max,
                step: None,
            },
            default: ParamValue::Float(default),
        });
        self
    }

    /// A fixed, non-tunable parameter (e.g. a file path) with a string default.
    pub fn fixed_str(mut self, name: &str, default: &str) -> Self {
        self.entries.push(ParamEntry {
            name: name.to_string(),
            range: ParamRange::Fixed,
            default: ParamValue::Str(default.to_string()),
        });
        self
    }

    /// A fixed, non-tunable integer parameter.
    pub fn fixed_int(mut self, name: &str, default: i64) -> Self {
        self.entries.push(ParamEntry {
            name: name.to_string(),
            range: ParamRange::Fixed,
            default: ParamValue::Int(default),
        });
        self
    }

    /// A fixed, non-tunable float parameter.
    pub fn fixed_float(mut self, name: &str, default: f64) -> Self {
        self.entries.push(ParamEntry {
            name: name.to_string(),
            range: ParamRange::Fixed,
            default: ParamValue::Float(default),
        });
        self
    }

    pub fn choice(mut self, name: &str, default: &str, options: &[&str]) -> Self {
        self.entries.push(ParamEntry {
            name: name.to_string(),
            range: ParamRange::Choice(options.iter().map(|s| s.to_string()).collect()),
            default: ParamValue::Str(default.to_string()),
        });
        self
    }

    /// Build a [`Params`] holding the declared defaults.
    pub fn defaults(&self) -> Params {
        let mut p = Params::default();
        for e in &self.entries {
            p.0.insert(e.name.clone(), e.default.clone());
        }
        p
    }
}

/// A concrete map of parameter values. Keys are local to a component (e.g. `window`); the
/// hyperopt layer namespaces them across stages (e.g. `indicator.technical.rsi_period`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Params(pub BTreeMap<String, ParamValue>);

impl Params {
    pub fn get(&self, key: &str) -> Option<&ParamValue> {
        self.0.get(key)
    }

    pub fn set(&mut self, key: &str, value: ParamValue) {
        self.0.insert(key.to_string(), value);
    }

    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.0
            .get(key)
            .and_then(ParamValue::as_i64)
            .unwrap_or(default)
    }

    pub fn usize(&self, key: &str, default: usize) -> usize {
        self.int(key, default as i64).max(0) as usize
    }

    pub fn float(&self, key: &str, default: f64) -> f64 {
        self.0
            .get(key)
            .and_then(ParamValue::as_f64)
            .unwrap_or(default)
    }

    pub fn str(&self, key: &str, default: &str) -> String {
        self.0
            .get(key)
            .and_then(ParamValue::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| default.to_string())
    }

    /// Overlay `overrides` on top of `self`, returning a new map (overrides win).
    pub fn merged_with(&self, overrides: &Params) -> Params {
        let mut out = self.clone();
        for (k, v) in &overrides.0 {
            out.0.insert(k.clone(), v.clone());
        }
        out
    }
}

/// A component that can be described as a search space and built from concrete parameters.
pub trait Configurable: Sized {
    /// The declared, tunable parameters (with defaults).
    fn param_spec() -> ParamSpec;

    /// Build an instance. `params` is expected to be fully resolved (spec defaults overlaid
    /// with user/hyperopt overrides), so every declared key is present.
    fn from_params(params: &Params) -> Self;

    /// Convenience: resolve raw overrides against the spec defaults, then build.
    fn build(overrides: &Params) -> Self {
        let resolved = Self::param_spec().defaults().merged_with(overrides);
        Self::from_params(&resolved)
    }
}
