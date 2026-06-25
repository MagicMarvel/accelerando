//! Run configuration (`run.toml`) and conversion of TOML parameter tables into [`Params`].

use accelerando_core::{ParamValue, Params};
use serde::{Deserialize, Serialize};

/// One pluggable stage: which adapter, plus its override parameters.
#[derive(Clone, Debug, Deserialize)]
pub struct StageCfg {
    pub adapter: String,
    #[serde(default)]
    pub params: toml::Table,
}

/// Broker simulation settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BrokerCfg {
    #[serde(default)]
    pub commission_per_contract: f64,
    #[serde(default)]
    pub slippage_ticks: f64,
    #[serde(default = "default_equity")]
    pub starting_equity: f64,
}

impl Default for BrokerCfg {
    fn default() -> Self {
        Self {
            commission_per_contract: 0.0,
            slippage_ticks: 0.0,
            starting_equity: default_equity(),
        }
    }
}

fn default_equity() -> f64 {
    100_000.0
}
fn default_true() -> bool {
    true
}
fn default_result() -> String {
    "result.json".to_string()
}

/// The full backtest configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct RunConfig {
    #[serde(default = "default_result")]
    pub result: String,
    #[serde(default = "default_true")]
    pub keep_footprints: bool,
    pub data: StageCfg,
    pub aggregator: StageCfg,
    #[serde(default)]
    pub indicator: Vec<StageCfg>,
    pub strategy: StageCfg,
    #[serde(default)]
    pub broker: BrokerCfg,
}

impl RunConfig {
    pub fn load(path: &str) -> Result<RunConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        toml::from_str(&text).map_err(|e| format!("parse {path}: {e}"))
    }
}

/// Convert a TOML parameter table into a [`Params`] override map.
pub fn params_from_table(table: &toml::Table) -> Params {
    let mut p = Params::default();
    for (k, v) in table {
        let pv = match v {
            toml::Value::Integer(i) => ParamValue::Int(*i),
            toml::Value::Float(f) => ParamValue::Float(*f),
            toml::Value::String(s) => ParamValue::Str(s.clone()),
            toml::Value::Boolean(b) => ParamValue::Int(if *b { 1 } else { 0 }),
            _ => continue, // ignore arrays/tables/datetimes as params
        };
        p.0.insert(k.clone(), pv);
    }
    p
}
