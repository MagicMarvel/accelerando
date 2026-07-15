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

    /// Parse repeated `name=value` assignments according to the declared parameter types.
    ///
    /// String splitting may happen at an application's CLI boundary, but interpreting a value as
    /// an integer, float, choice, or fixed value is framework-owned because only `ParamSpec` knows
    /// those semantics.
    pub fn parse_assignments(&self, assignments: &[String]) -> Result<Params, String> {
        let mut overrides = Params::default();
        for assignment in assignments {
            let (name, raw) = assignment.split_once('=').ok_or_else(|| {
                format!("invalid parameter assignment `{assignment}`; expected name=value")
            })?;
            let name = name.trim();
            let raw = raw.trim();
            if name.is_empty() {
                return Err(format!(
                    "invalid parameter assignment `{assignment}`; parameter name is empty"
                ));
            }
            if overrides.get(name).is_some() {
                return Err(format!("parameter `{name}` was specified more than once"));
            }
            let entry = self.entry(name)?;
            let value =
                match (&entry.range, &entry.default) {
                    (ParamRange::Int { .. }, _) | (ParamRange::Fixed, ParamValue::Int(_)) => {
                        ParamValue::Int(raw.parse::<i64>().map_err(|_| {
                            format!("parameter `{name}` expects an integer, got `{raw}`")
                        })?)
                    }
                    (ParamRange::Float { .. }, _) | (ParamRange::Fixed, ParamValue::Float(_)) => {
                        ParamValue::Float(raw.parse::<f64>().map_err(|_| {
                            format!("parameter `{name}` expects a number, got `{raw}`")
                        })?)
                    }
                    (ParamRange::Choice(_), _) | (ParamRange::Fixed, ParamValue::Str(_)) => {
                        ParamValue::Str(raw.to_string())
                    }
                };
            overrides.set(name, value);
        }
        self.validate(&overrides)?;
        Ok(overrides)
    }

    /// Validate overrides and return a complete parameter map with defaults filled in.
    pub fn resolve(&self, overrides: &Params) -> Result<Params, String> {
        self.validate(overrides)?;
        Ok(self.defaults().merged_with(overrides))
    }

    /// Check every override against this spec, including names, types, numeric ranges/steps, and
    /// choice membership. Returns a human-readable error instead of silently using a default.
    pub fn validate(&self, overrides: &Params) -> Result<(), String> {
        for (key, value) in &overrides.0 {
            let entry = self.entry(key)?;
            match (&entry.range, value, &entry.default) {
                (ParamRange::Int { min, max, step }, ParamValue::Int(v), _) => {
                    if v < min || v > max {
                        return Err(format!("parameter `{key}`={v} is outside [{min}, {max}]"));
                    }
                    if *step > 0 && (v - min) % step != 0 {
                        return Err(format!(
                            "parameter `{key}`={v} does not align to step {step} from {min}"
                        ));
                    }
                }
                (ParamRange::Int { .. }, _, _) => {
                    return Err(format!("parameter `{key}` must be an integer"));
                }
                (ParamRange::Float { min, max, .. }, ParamValue::Float(v), _) => {
                    let v = *v;
                    if !v.is_finite() || v < *min || v > *max {
                        return Err(format!("parameter `{key}`={v} is outside [{min}, {max}]"));
                    }
                }
                (ParamRange::Float { min, max, .. }, ParamValue::Int(v), _) => {
                    let v = *v as f64;
                    if !v.is_finite() || v < *min || v > *max {
                        return Err(format!("parameter `{key}`={v} is outside [{min}, {max}]"));
                    }
                }
                (ParamRange::Float { .. }, _, _) => {
                    return Err(format!("parameter `{key}` must be a number"));
                }
                (ParamRange::Choice(options), ParamValue::Str(chosen), _) => {
                    if !options.iter().any(|option| option == chosen) {
                        return Err(format!(
                            "parameter `{key}` has invalid value `{chosen}`; valid options: {}",
                            options.join(", ")
                        ));
                    }
                }
                (ParamRange::Choice(options), _, _) => {
                    return Err(format!(
                        "parameter `{key}` is a choice and must be a string (one of: {})",
                        options.join(", ")
                    ));
                }
                (ParamRange::Fixed, ParamValue::Int(_), ParamValue::Int(_))
                | (ParamRange::Fixed, ParamValue::Str(_), ParamValue::Str(_)) => {}
                (ParamRange::Fixed, ParamValue::Float(v), ParamValue::Float(_)) => {
                    if !v.is_finite() {
                        return Err(format!("parameter `{key}` must be finite"));
                    }
                }
                (ParamRange::Fixed, _, expected) => {
                    let expected = match expected {
                        ParamValue::Int(_) => "an integer",
                        ParamValue::Float(_) => "a number",
                        ParamValue::Str(_) => "a string",
                    };
                    return Err(format!("parameter `{key}` must be {expected}"));
                }
            }
        }
        Ok(())
    }

    fn entry(&self, name: &str) -> Result<&ParamEntry, String> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| {
                let mut known: Vec<&str> = self
                    .entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect();
                known.sort_unstable();
                format!(
                    "unknown parameter `{name}`; declared parameters: {}",
                    known.join(", ")
                )
            })
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
    ///
    /// Panics if `overrides` contains a key the spec does not declare or an invalid choice
    /// value. A typo'd parameter name silently falling back to its default is the worst
    /// failure mode a backtest framework can have, so misconfiguration fails loudly here.
    fn build(overrides: &Params) -> Self {
        let spec = Self::param_spec();
        let resolved = spec
            .resolve(overrides)
            .unwrap_or_else(|err| panic!("invalid params: {err}"));
        Self::from_params(&resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ParamSpec {
        ParamSpec::new()
            .int("window", 20, 5, 50, 5)
            .float("threshold", 1.5, 0.0, 3.0)
            .choice("side", "both", &["both", "long"])
            .fixed_float("tick", 0.25)
    }

    #[test]
    fn framework_parses_validates_and_resolves_assignments() {
        let assignments = vec!["window=30".to_string(), "side=long".to_string()];
        let overrides = spec().parse_assignments(&assignments).unwrap();
        let resolved = spec().resolve(&overrides).unwrap();

        assert_eq!(resolved.int("window", 0), 30);
        assert_eq!(resolved.str("side", ""), "long");
        assert_eq!(resolved.float("threshold", 0.0), 1.5);
        assert_eq!(resolved.float("tick", 0.0), 0.25);
    }

    #[test]
    fn framework_rejects_invalid_types_ranges_and_steps() {
        for assignment in ["window=31", "threshold=text", "side=short"] {
            assert!(spec().parse_assignments(&[assignment.to_string()]).is_err());
        }
    }
}
