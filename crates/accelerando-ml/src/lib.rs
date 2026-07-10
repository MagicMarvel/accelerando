//! Factor-mining helpers built on the [`perpetual`] gradient-boosting crate.
//!
//! The workflow this crate supports: a downstream runner extracts one feature vector per
//! footprint plus a forward-return label into a [`FactorTable`], then calls
//! [`evaluate_factors`] to get a [`FactorReport`] — per-factor coverage, univariate Spearman
//! IC against the label, and PerpetualBooster feature importance from a chronological
//! train/test fit — so weak factors can be culled and strong ones promoted into strategies.
//!
//! PerpetualBooster handles `NaN` feature values natively (learnable missing-value splits),
//! so sparse factors (e.g. order-book features on a trades-only feed) can stay in the table;
//! columns below a coverage floor are dropped and reported instead of silently fit.

use std::collections::HashMap;

use perpetual::booster::config::ImportanceMethod;
use perpetual::objective::Objective;
use perpetual::{Matrix, PerpetualBooster};

/// Column-major table of factor values plus one label per row.
///
/// Rows are expected to be pushed in chronological order; [`evaluate_factors`] splits
/// train/test by row position, not randomly, to avoid look-ahead leakage.
pub struct FactorTable {
    feature_names: Vec<String>,
    /// One `Vec` per feature, all `rows` long.
    columns: Vec<Vec<f64>>,
    label: Vec<f64>,
}

impl FactorTable {
    pub fn new(feature_names: &[&str]) -> Self {
        Self {
            feature_names: feature_names.iter().map(|s| s.to_string()).collect(),
            columns: vec![Vec::new(); feature_names.len()],
            label: Vec::new(),
        }
    }

    /// Append one observation. `features` must match the constructor's name count;
    /// unknown/unavailable values are `f64::NAN`.
    pub fn push_row(&mut self, features: &[f64], label: f64) {
        assert_eq!(
            features.len(),
            self.columns.len(),
            "feature vector length must match feature_names"
        );
        for (column, &value) in self.columns.iter_mut().zip(features) {
            column.push(value);
        }
        self.label.push(label);
    }

    pub fn rows(&self) -> usize {
        self.label.len()
    }

    pub fn feature_names(&self) -> &[String] {
        &self.feature_names
    }

    /// Fraction of non-NaN entries in a feature column.
    fn coverage(&self, col: usize) -> f64 {
        if self.label.is_empty() {
            return 0.0;
        }
        let finite = self.columns[col].iter().filter(|v| v.is_finite()).count();
        finite as f64 / self.label.len() as f64
    }

    /// Write the table as CSV (header + rows) for offline analysis.
    pub fn write_csv(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let file = std::fs::File::create(path)?;
        let mut out = std::io::BufWriter::new(file);
        writeln!(out, "{},label", self.feature_names.join(","))?;
        for row in 0..self.rows() {
            for column in &self.columns {
                let v = column[row];
                if v.is_finite() {
                    write!(out, "{v},")?;
                } else {
                    write!(out, ",")?;
                }
            }
            writeln!(out, "{}", self.label[row])?;
        }
        Ok(())
    }
}

/// Everything the report knows about one surviving factor.
#[derive(Clone, Debug)]
pub struct FactorStat {
    pub name: String,
    /// Fraction of rows where the factor had a finite value.
    pub coverage: f64,
    /// Spearman rank correlation with the label over all finite pairs.
    pub ic: f64,
    /// Normalized PerpetualBooster importance (total gain), sums to ~1 over kept factors.
    pub importance: f64,
}

/// Result of [`evaluate_factors`].
#[derive(Clone, Debug)]
pub struct FactorReport {
    /// Factors that entered the model, sorted by importance descending.
    pub factors: Vec<FactorStat>,
    /// (name, coverage) of factors dropped for insufficient coverage.
    pub dropped: Vec<(String, f64)>,
    pub train_rows: usize,
    pub test_rows: usize,
    /// Out-of-sample R² of the model on the chronological test tail.
    pub test_r2: f64,
    /// Sign hit rate on test rows whose label is nonzero.
    pub test_hit_rate: f64,
    /// Hit rate of always guessing the training set's majority label sign.
    pub baseline_hit_rate: f64,
    /// Pearson correlation between prediction and label on the test tail.
    pub test_pred_corr: f64,
}

/// Fit PerpetualBooster on the chronological head of `table` and score the tail.
///
/// * `train_frac` — fraction of rows (from the start) used for fitting, e.g. `0.7`.
/// * `budget` — Perpetual's single complexity knob; `1.0` is the library default.
/// * `min_coverage` — features with fewer finite rows than this fraction are dropped.
pub fn evaluate_factors(
    table: &FactorTable,
    train_frac: f64,
    budget: f32,
    min_coverage: f64,
) -> Result<FactorReport, String> {
    let rows = table.rows();
    if rows < 100 {
        return Err(format!("factor table too small: {rows} rows"));
    }

    let mut kept_cols: Vec<usize> = Vec::new();
    let mut dropped: Vec<(String, f64)> = Vec::new();
    for col in 0..table.columns.len() {
        let coverage = table.coverage(col);
        if coverage >= min_coverage {
            kept_cols.push(col);
        } else {
            dropped.push((table.feature_names[col].clone(), coverage));
        }
    }
    if kept_cols.is_empty() {
        return Err("no factor column meets the coverage floor".to_string());
    }

    let train_rows = ((rows as f64) * train_frac.clamp(0.05, 0.95)).round() as usize;
    let test_rows = rows - train_rows;
    if train_rows < 50 || test_rows < 50 {
        return Err(format!(
            "degenerate split: {train_rows} train / {test_rows} test rows"
        ));
    }

    // Column-major flattening, matching perpetual's Matrix layout.
    let flatten = |row_range: std::ops::Range<usize>| -> Vec<f64> {
        let mut data = Vec::with_capacity(kept_cols.len() * row_range.len());
        for &col in &kept_cols {
            data.extend_from_slice(&table.columns[col][row_range.clone()]);
        }
        data
    };
    let train_data = flatten(0..train_rows);
    let test_data = flatten(train_rows..rows);
    let y_train = &table.label[0..train_rows];
    let y_test = &table.label[train_rows..rows];

    let train_matrix = Matrix::new(&train_data, train_rows, kept_cols.len());
    let test_matrix = Matrix::new(&test_data, test_rows, kept_cols.len());

    let mut model = PerpetualBooster::default()
        .set_objective(Objective::SquaredLoss)
        .set_budget(budget);
    model
        .fit(&train_matrix, y_train, None, None)
        .map_err(|e| format!("PerpetualBooster fit failed: {e}"))?;

    let preds = model.predict(&test_matrix, true);
    let importance: HashMap<usize, f32> =
        model.calculate_feature_importance(ImportanceMethod::TotalGain, true);

    let mean_y = mean(y_test);
    let ss_tot: f64 = y_test.iter().map(|y| (y - mean_y).powi(2)).sum();
    let ss_res: f64 = y_test
        .iter()
        .zip(&preds)
        .map(|(y, p)| (y - p).powi(2))
        .sum();
    let test_r2 = if ss_tot > 0.0 {
        1.0 - ss_res / ss_tot
    } else {
        0.0
    };

    let mut hits = 0usize;
    let mut calls = 0usize;
    for (y, p) in y_test.iter().zip(&preds) {
        if *y != 0.0 {
            calls += 1;
            if (y > &0.0) == (p > &0.0) {
                hits += 1;
            }
        }
    }
    let test_hit_rate = if calls > 0 {
        hits as f64 / calls as f64
    } else {
        0.0
    };

    let up_train = y_train.iter().filter(|y| **y > 0.0).count();
    let down_train = y_train.iter().filter(|y| **y < 0.0).count();
    let majority_up = up_train >= down_train;
    let mut base_hits = 0usize;
    for y in y_test.iter().filter(|y| **y != 0.0) {
        if (*y > 0.0) == majority_up {
            base_hits += 1;
        }
    }
    let baseline_hit_rate = if calls > 0 {
        base_hits as f64 / calls as f64
    } else {
        0.0
    };

    let test_pred_corr = pearson(&preds, y_test);

    let mut factors: Vec<FactorStat> = kept_cols
        .iter()
        .enumerate()
        .map(|(model_idx, &col)| FactorStat {
            name: table.feature_names[col].clone(),
            coverage: table.coverage(col),
            ic: spearman(&table.columns[col], &table.label),
            importance: importance.get(&model_idx).copied().unwrap_or(0.0) as f64,
        })
        .collect();
    factors.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(FactorReport {
        factors,
        dropped,
        train_rows,
        test_rows,
        test_r2,
        test_hit_rate,
        baseline_hit_rate,
        test_pred_corr,
    })
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let pairs: Vec<(f64, f64)> = a
        .iter()
        .zip(b)
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .map(|(x, y)| (*x, *y))
        .collect();
    if pairs.len() < 3 {
        return f64::NAN;
    }
    let n = pairs.len() as f64;
    let mx = pairs.iter().map(|(x, _)| x).sum::<f64>() / n;
    let my = pairs.iter().map(|(_, y)| y).sum::<f64>() / n;
    let mut cov = 0.0;
    let mut vx = 0.0;
    let mut vy = 0.0;
    for (x, y) in &pairs {
        cov += (x - mx) * (y - my);
        vx += (x - mx).powi(2);
        vy += (y - my).powi(2);
    }
    if vx <= 0.0 || vy <= 0.0 {
        return 0.0;
    }
    cov / (vx.sqrt() * vy.sqrt())
}

/// Spearman rank correlation over rows where both series are finite (average ranks for ties).
pub fn spearman(a: &[f64], b: &[f64]) -> f64 {
    let pairs: Vec<(f64, f64)> = a
        .iter()
        .zip(b)
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .map(|(x, y)| (*x, *y))
        .collect();
    if pairs.len() < 3 {
        return f64::NAN;
    }
    let xs: Vec<f64> = pairs.iter().map(|(x, _)| *x).collect();
    let ys: Vec<f64> = pairs.iter().map(|(_, y)| *y).collect();
    pearson(&ranks(&xs), &ranks(&ys))
}

fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&i, &j| {
        values[i]
            .partial_cmp(&values[j])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && values[order[j + 1]] == values[order[i]] {
            j += 1;
        }
        // Average rank for the tie run [i, j].
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for &idx in &order[i..=j] {
            out[idx] = avg;
        }
        i = j + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spearman_detects_monotone_relation() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let b = vec![10.0, 20.0, 25.0, 40.0, 100.0];
        assert!((spearman(&a, &b) - 1.0).abs() < 1e-9);
        let inv: Vec<f64> = b.iter().map(|v| -v).collect();
        assert!((spearman(&a, &inv) + 1.0).abs() < 1e-9);
    }

    #[test]
    fn ranks_average_ties() {
        assert_eq!(ranks(&[1.0, 2.0, 2.0, 3.0]), vec![1.0, 2.5, 2.5, 4.0]);
    }

    #[test]
    fn evaluate_learns_a_planted_signal() {
        let mut table = FactorTable::new(&["signal", "noise", "empty"]);
        let mut state = 42u64;
        let mut rng = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        };
        for _ in 0..2000 {
            let signal = rng();
            let noise = rng();
            let label = 2.0 * signal + 0.1 * rng();
            table.push_row(&[signal, noise, f64::NAN], label);
        }
        let report = evaluate_factors(&table, 0.7, 0.5, 0.05).expect("fit succeeds");
        assert_eq!(report.dropped.len(), 1, "all-NaN column is dropped");
        assert_eq!(report.factors[0].name, "signal");
        assert!(report.factors[0].ic > 0.9);
        assert!(report.test_r2 > 0.5, "test R² {}", report.test_r2);
    }
}
