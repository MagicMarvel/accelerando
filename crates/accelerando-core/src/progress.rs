//! A lightweight, thread-shareable progress handle.
//!
//! The engine increments `footprints` and flips `done`; a data source that knows its input size
//! reports `bytes` against `total_bytes`. A server polling thread reads [`ProgressHandle::snapshot`]
//! to drive a progress bar while a backtest runs on a worker thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Shared, cloneable progress counters for a running backtest.
#[derive(Clone, Default)]
pub struct ProgressHandle {
    bytes: Arc<AtomicU64>,
    total_bytes: Arc<AtomicU64>,
    footprints: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
}

/// A point-in-time snapshot of progress.
#[derive(Clone, Copy, Debug)]
pub struct ProgressSnapshot {
    pub bytes: u64,
    pub total_bytes: u64,
    pub footprints: u64,
    pub done: bool,
}

impl ProgressSnapshot {
    /// Fraction complete in [0, 1], or `None` if the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        if self.total_bytes > 0 {
            Some((self.bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

impl ProgressHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_total_bytes(&self, n: u64) {
        self.total_bytes.store(n, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_footprints(&self) {
        self.footprints.fetch_add(1, Ordering::Relaxed);
    }

    pub fn finish(&self) {
        self.done.store(true, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            bytes: self.bytes.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            footprints: self.footprints.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
        }
    }
}
