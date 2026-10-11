// SPDX-License-Identifier: MIT OR Apache-2.0

//! Counter helpers. Counters are monotonic `u64`s; the atomic form is for
//! values one thread writes and others read (relaxed, tear-free on arm64).

use std::sync::atomic::{AtomicU64, Ordering};

/// A relaxed atomic counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn inc(&self) {
        self.add(1);
    }

    /// Adds `n` (wrapping, which a u64 event counter never reaches).
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Gauge: keep the maximum seen.
    pub fn set_max(&self, v: u64) {
        self.0.fetch_max(v, Ordering::Relaxed);
    }

    pub fn set(&self, v: u64) {
        self.0.store(v, Ordering::Relaxed);
    }
}

/// Saturating increment of a plain (single-owner) counter.
pub fn bump(c: &mut u64) {
    *c = c.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_ops() {
        let c = Counter::new();
        c.inc();
        c.add(4);
        assert_eq!(c.get(), 5);
        c.set_max(3);
        assert_eq!(c.get(), 5);
        c.set_max(9);
        assert_eq!(c.get(), 9);
        c.set(1);
        assert_eq!(c.get(), 1);
        let mut p = u64::MAX - 1;
        bump(&mut p);
        bump(&mut p);
        assert_eq!(p, u64::MAX);
    }
}
