use crate::{ParamValue, Params};

/// The aggregation a strategy expects. Units are explicit and built-in names are checked
/// by the compiler. CLI text is parsed by the runner before constructing an aggregator.
///
/// ```
/// use accelerando_core::{BarSpec, StrategyRequirements};
/// let requirements = StrategyRequirements::new().bars(BarSpec::Time { seconds: 1 });
/// assert_eq!(requirements.bars, Some(BarSpec::Time { seconds: 1 }));
/// ```
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BarSpec {
    Time {
        seconds: i64,
    },
    Range {
        ticks: i64,
    },
    Reverse {
        ticks: i64,
    },
    /// Whole traded contracts; the built-in volume aggregator accepts integer volume.
    Volume {
        contracts: i64,
    },
    Custom {
        name: String,
        params: Params,
    },
}

impl BarSpec {
    /// Convert a typed requirement to registry parameters, preserving numeric types.
    pub fn component(&self) -> (&str, Params) {
        match self {
            Self::Time { seconds } => ("time", Params::default().with("bar_secs", *seconds)),
            Self::Range { ticks } => ("range", Params::default().with("range_ticks", *ticks)),
            Self::Reverse { ticks } => {
                ("reverse", Params::default().with("reversal_ticks", *ticks))
            }
            Self::Volume { contracts } => ("volume", Params::default().with("volume", *contracts)),
            Self::Custom { name, params } => (name, params.clone()),
        }
    }

    /// Normalize resolved registry parameters. Additional custom parameters are retained.
    pub fn from_component(name: &str, params: Params) -> Self {
        if params.0.len() == 1 {
            match name {
                "time" => {
                    if let Some(ParamValue::Int(seconds)) = params.get("bar_secs") {
                        return Self::Time { seconds: *seconds };
                    }
                }
                "range" => {
                    if let Some(ParamValue::Int(ticks)) = params.get("range_ticks") {
                        return Self::Range { ticks: *ticks };
                    }
                }
                "reverse" => {
                    if let Some(ParamValue::Int(ticks)) = params.get("reversal_ticks") {
                        return Self::Reverse { ticks: *ticks };
                    }
                }
                "volume" => {
                    if let Some(ParamValue::Int(contracts)) = params.get("volume") {
                        return Self::Volume {
                            contracts: *contracts,
                        };
                    }
                }
                _ => {}
            }
        }
        Self::Custom {
            name: name.to_owned(),
            params,
        }
    }
}

impl std::fmt::Display for BarSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Time { seconds } => write!(f, "time:{seconds}"),
            Self::Range { ticks } => write!(f, "range:{ticks}"),
            Self::Reverse { ticks } => write!(f, "reverse:{ticks}"),
            Self::Volume { contracts } => write!(f, "volume:{contracts}"),
            Self::Custom { name, params } => {
                write!(f, "{name}")?;
                for (index, (key, value)) in params.0.iter().enumerate() {
                    write!(f, "{}{key}=", if index == 0 { ":" } else { "," })?;
                    match value {
                        ParamValue::Int(value) => write!(f, "{value}")?,
                        ParamValue::Float(value) => write!(f, "{value}")?,
                        ParamValue::Str(value) => write!(f, "{value}")?,
                    }
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_round_trip_through_numeric_registry_parameters() {
        for spec in [
            BarSpec::Time { seconds: 1 },
            BarSpec::Range { ticks: 30 },
            BarSpec::Reverse { ticks: 16 },
            BarSpec::Volume { contracts: 1000 },
        ] {
            let (name, params) = spec.component();
            assert_eq!(BarSpec::from_component(name, params), spec);
            let json = serde_json::to_string(&spec).unwrap();
            assert_eq!(serde_json::from_str::<BarSpec>(&json).unwrap(), spec);
        }
    }

    #[test]
    fn custom_parameters_are_not_lost_during_normalization() {
        let params = Params::default()
            .with("bar_secs", 1)
            .with("custom_option", true);
        let spec = BarSpec::from_component("time", params.clone());
        assert_eq!(
            spec,
            BarSpec::Custom {
                name: "time".into(),
                params: params.clone()
            }
        );
        assert_eq!(spec.component(), ("time", params));
    }
}
