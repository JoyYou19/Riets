//! Lightweight aggregated function-timing registry.
//!
//! Apply `#[timed]` to any function (sync or async, free fn or method).
//! When the crate's `timing` feature is off, the instrumentation compiles
//! to nothing. When it's on, each call updates an in-memory aggregate
//! (count / total / min / max) keyed by function name. Call [`report`]
//! from a debug command, admin endpoint, or test to see the results.
//!
//! This is intentionally decoupled from `tracing` — it doesn't need a
//! subscriber installed, and it aggregates instead of emitting one
//! event/span per call. Use it alongside `#[tracing::instrument]` if you
//! also want per-call spans in your existing tracing output.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub use core_timing_macros::timed;

#[derive(Debug, Default, Clone, Copy)]
pub struct FnStats {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
}

impl FnStats {
    pub fn avg_ns(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.count as f64
        }
    }
}

fn registry() -> &'static Mutex<HashMap<&'static str, FnStats>> {
    static REGISTRY: OnceLock<Mutex<HashMap<&'static str, FnStats>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called by the `#[timed]` macro. Not usually called directly.
///
/// Note: this takes a plain `Mutex<HashMap<..>>` lock per call, which is
/// fine for dev-time profiling. If you end up applying `#[timed]` to a
/// function called millions of times per second across many shard
/// threads and see contention, prefer timing the boundary functions
/// (parse, index-write, disk-write) rather than every inner helper.
pub fn record(name: &'static str, elapsed: Duration) {
    let ns = elapsed.as_nanos() as u64;
    let mut map = registry().lock().unwrap();
    let entry = map.entry(name).or_insert(FnStats {
        count: 0,
        total_ns: 0,
        min_ns: u64::MAX,
        max_ns: 0,
    });
    entry.count += 1;
    entry.total_ns += ns;
    if ns < entry.min_ns {
        entry.min_ns = ns;
    }
    if ns > entry.max_ns {
        entry.max_ns = ns;
    }
}

/// Snapshot of all recorded stats, sorted by total time descending
/// (the functions worth looking at first).
pub fn snapshot() -> Vec<(&'static str, FnStats)> {
    let map = registry().lock().unwrap();
    let mut v: Vec<_> = map.iter().map(|(&k, &s)| (k, s)).collect();
    v.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
    v
}

/// Clear all recorded stats — useful between benchmark runs.
pub fn reset() {
    registry().lock().unwrap().clear();
}

/// Human-readable table, e.g. to return from a debug command/endpoint.
pub fn report() -> String {
    let snap = snapshot();
    if snap.is_empty() {
        return "No timing data recorded (is the `timing` feature enabled for this build?)"
            .to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<32} {:>8} {:>12} {:>12} {:>12} {:>12}\n",
        "function", "calls", "total", "avg", "min", "max"
    ));
    for (name, s) in snap {
        out.push_str(&format!(
            "{:<32} {:>8} {:>12} {:>12} {:>12} {:>12}\n",
            name,
            s.count,
            fmt_duration(s.total_ns),
            fmt_duration(s.avg_ns() as u64),
            fmt_duration(s.min_ns),
            fmt_duration(s.max_ns),
        ));
    }
    out
}

/// Time an arbitrary expression without needing to own its definition —
/// useful for a single call site (a function in a dependency crate you
/// can't annotate, a specific `.await`, a specific loop) rather than a
/// whole function. Works for sync and async expressions alike, and plays
/// correctly with `?`/early return inside the expression.
///
/// ```ignore
/// let command = core_timing::timed_block!(
///     "search_command_parse",
///     SearchCommand::parse(&body, ctx.format)?
/// );
///
/// let hits = core_timing::timed_block!("shard_search", manager.search(&command).await)?;
/// ```
///
/// Same zero-cost-when-off behavior as `#[timed]`: requires the *calling*
/// crate to declare its own `timing` feature (see the crate docs / README).
#[macro_export]
macro_rules! timed_block {
    ($label:expr, $body:expr) => {{
        if cfg!(feature = "timing") {
            let __perf_start = ::std::time::Instant::now();
            let __perf_result = $body;
            $crate::record($label, __perf_start.elapsed());
            __perf_result
        } else {
            $body
        }
    }};
}

fn fmt_duration(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.2}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}
