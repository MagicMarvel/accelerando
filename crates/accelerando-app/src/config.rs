//! `accelerando.toml`: per-project defaults for every CLI flag, so day-to-day runs are just
//! `cargo run --release` (plus whatever you want to change this time).
//!
//! ```toml
//! data = "C:/data/ESZ6.parquet"       # the positional data path
//! strategy = "vp_poc_return"
//! slippage_ticks = 1
//! gui = true                             # boolean flags: true = pass the flag
//! indicator = ["regime", "big_trades:min_trade_size=20"]   # repeatable flags take arrays
//!
//! [params]                               # --param name=value, for `strategy`
//! reward_risk = 1.5
//! entry_start = "18:00"
//! ```
//!
//! Keys are the long flag names with `-` or `_` (`slippage_ticks` = `--slippage-ticks`).
//! The command line wins over the file: a flag given on the CLI replaces the file's value, a CLI
//! `--param x=..` replaces the file's `x`, and choosing a different `--strategy` on the CLI drops
//! the file's `[params]` (they belong to the file's strategy).

use std::path::Path;

/// Default file name, looked up in the working directory.
pub const CONFIG_FILE: &str = "accelerando.toml";

/// Turn a config file into the argument list it stands for.
pub(crate) fn args_from_file(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    args_from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn args_from_str(text: &str) -> Result<Vec<String>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut args = Vec::new();
    for (key, value) in &table {
        let flag = format!("--{}", key.replace('_', "-"));
        match (key.as_str(), value) {
            ("data", v) => args.push(scalar(key, v)?),
            ("params", toml::Value::Table(params)) => {
                for (name, v) in params {
                    args.push("--param".into());
                    args.push(format!("{name}={}", scalar(name, v)?));
                }
            }
            ("params", _) => return Err("`params` must be a table: [params]".into()),
            (_, toml::Value::Boolean(on)) => {
                if *on {
                    args.push(flag);
                } else if let Some(off) = negation(key) {
                    args.push(off.into());
                }
            }
            (_, toml::Value::Array(items)) => {
                for v in items {
                    args.push(flag.clone());
                    args.push(scalar(key, v)?);
                }
            }
            (_, v) => {
                args.push(flag);
                args.push(scalar(key, v)?);
            }
        }
    }
    Ok(args)
}

/// Flags whose `false` means something (the default is on, or the file may want to turn a
/// default-on behaviour off).
fn negation(key: &str) -> Option<&'static str> {
    match key.replace('-', "_").as_str() {
        "grid" => Some("--no-grid"),
        "cache" => Some("--no-cache"),
        _ => None,
    }
}

fn scalar(key: &str, v: &toml::Value) -> Result<String, String> {
    Ok(match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => return Err(format!("`{key}`: expected a string, number or bool, got {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_maps_to_flags() {
        let args = args_from_str(
            r#"
            data = "d.parquet"
            strategy = "s"
            slippage_ticks = 2
            gui = true
            serve = false
            cache = false
            indicator = ["a", "b:k=1"]
            [params]
            reward_risk = 1.5
            entry_start = "18:00"
            "#,
        )
        .unwrap();
        let joined = args.join(" ");
        for part in [
            "d.parquet",
            "--strategy s",
            "--slippage-ticks 2",
            "--gui",
            "--no-cache",
            "--indicator a --indicator b:k=1",
            "--param reward_risk=1.5",
            "--param entry_start=18:00",
        ] {
            assert!(joined.contains(part), "{part} missing from {joined}");
        }
        assert!(!joined.contains("serve"));
        assert!(args_from_str("params = 3").is_err());
    }
}
