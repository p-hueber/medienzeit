//! Measuring what actually blocks.
//!
//! The two-core split is justified by numbers that were, until this module, datasheet
//! typicals and guesses. A blocking call is cheap to time and expensive to argue about,
//! so the ones that decide the design get measured and stay measured — a regression here
//! is the kind that shows up as "the web page feels slow" three weeks later.

use embassy_time::Instant;
use esp_println::println;

/// Accumulates durations and reports a summary every `every` calls.
///
/// For calls too frequent to log individually. Rare ones are timed at the call site,
/// where the surrounding log line already says what happened.
pub struct Stats {
    what: &'static str,
    every: u32,
    n: u32,
    total_us: u64,
    max_us: u64,
}

impl Stats {
    pub const fn new(what: &'static str, every: u32) -> Self {
        Self { what, every, n: 0, total_us: 0, max_us: 0 }
    }

    /// Time one call, folding it into the summary.
    pub fn time<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        self.record(started.elapsed().as_micros());
        out
    }

    fn record(&mut self, us: u64) {
        self.n += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        if self.n < self.every {
            return;
        }
        let avg = self.total_us / self.n as u64;
        println!(
            "timing: {} n={} avg={} max={}",
            self.what,
            self.n,
            Ms(avg),
            Ms(self.max_us)
        );
        self.n = 0;
        self.total_us = 0;
        self.max_us = 0;
    }
}

/// Microseconds, printed as milliseconds to one decimal.
///
/// Whole milliseconds would round the sub-millisecond calls to zero, which is the
/// difference between "cheap" and "not measured".
pub struct Ms(pub u64);

impl core::fmt::Display for Ms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}ms", self.0 / 1000, (self.0 % 1000) / 100)
    }
}
