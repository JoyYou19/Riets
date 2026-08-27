//! Lightweight aggregated function-timing registry.
//!
//! Apply `#[timed(category)]` to any function (sync or async, free fn or
//! method). When the crate's `timing` feature is off, the instrumentation
//! compiles to nothing. When it's on, each call updates an in-memory
//! aggregate (count / total / min / max / baseline) keyed by
//! `(category, function name)`. Call [`report`] from a debug command,
//! admin endpoint, or test to see the results, grouped by category.
//!
//! This is intentionally decoupled from `tracing` — it doesn't need a
//! subscriber installed, and it aggregates instead of emitting one
//! event/span per call. Use it alongside `#[tracing::instrument]` if you
//! also want per-call spans in your existing tracing output.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub use core_timing_macros::timed;

/// Once a function has been called this many times, its average at that
/// point is frozen as the "baseline" for that function — later reports
/// show the current average's drift from it. Tune this if your call
/// volumes are much lower/higher than a few dozen per run.
pub const BASELINE_SAMPLE_SIZE: u64 = 20;

#[derive(Debug, Default, Clone, Copy)]
pub struct FnStats {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    /// Average duration (ns) as of the `BASELINE_SAMPLE_SIZE`-th call.
    /// `None` until that many calls have happened.
    pub baseline_avg_ns: Option<f64>,
}

impl FnStats {
    pub fn avg_ns(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.count as f64
        }
    }

    /// Percent change of the current average vs. the frozen baseline
    /// average. `None` if there's no baseline yet.
    pub fn drift_pct(&self) -> Option<f64> {
        let baseline = self.baseline_avg_ns?;
        if baseline <= 0.0 {
            return None;
        }
        Some((self.avg_ns() - baseline) / baseline * 100.0)
    }
}

type Key = (&'static str, &'static str); // (category, function name)

fn registry() -> &'static Mutex<HashMap<Key, FnStats>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Key, FnStats>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called by the `#[timed]` macro / `timed_block!`. Not usually called
/// directly.
///
/// Note: this takes a plain `Mutex<HashMap<..>>` lock per call, which is
/// fine for dev-time profiling. If you end up applying `#[timed]` to a
/// function called millions of times per second across many shard
/// threads and see contention, prefer timing the boundary functions
/// (parse, index-write, disk-write) rather than every inner helper.
pub fn record(category: &'static str, name: &'static str, elapsed: Duration) {
    let ns = elapsed.as_nanos() as u64;
    let mut map = registry().lock().unwrap();
    let entry = map.entry((category, name)).or_insert(FnStats {
        count: 0,
        total_ns: 0,
        min_ns: u64::MAX,
        max_ns: 0,
        baseline_avg_ns: None,
    });
    entry.count += 1;
    entry.total_ns += ns;
    if ns < entry.min_ns {
        entry.min_ns = ns;
    }
    if ns > entry.max_ns {
        entry.max_ns = ns;
    }
    if entry.count == BASELINE_SAMPLE_SIZE {
        entry.baseline_avg_ns = Some(entry.avg_ns());
    }
}

/// Snapshot grouped by category, each category's functions sorted by
/// total time descending, categories themselves sorted by their summed
/// total time descending (biggest time-sink category first).
pub fn snapshot() -> Vec<(&'static str, Vec<(&'static str, FnStats)>)> {
    let map = registry().lock().unwrap();
    let mut by_category: HashMap<&'static str, Vec<(&'static str, FnStats)>> = HashMap::new();
    for (&(category, name), &stats) in map.iter() {
        by_category.entry(category).or_default().push((name, stats));
    }

    let mut categories: Vec<(&'static str, Vec<(&'static str, FnStats)>)> =
        by_category.into_iter().collect();
    for (_, fns) in categories.iter_mut() {
        fns.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
    }
    categories.sort_by(|a, b| {
        let a_total: u64 = a.1.iter().map(|(_, s)| s.total_ns).sum();
        let b_total: u64 = b.1.iter().map(|(_, s)| s.total_ns).sum();
        b_total.cmp(&a_total)
    });
    categories
}

/// Same as [`snapshot`], but only the categories named in `categories`
/// (matched by exact string equality). An empty slice returns everything,
/// same as calling [`snapshot`] directly.
pub fn snapshot_filtered<S: AsRef<str>>(
    categories: &[S],
) -> Vec<(&'static str, Vec<(&'static str, FnStats)>)> {
    if categories.is_empty() {
        return snapshot();
    }
    snapshot()
        .into_iter()
        .filter(|(category, _)| categories.iter().any(|c| c.as_ref() == *category))
        .collect()
}

/// Clear all recorded stats (including baselines) — useful between
/// benchmark runs.
pub fn reset() {
    registry().lock().unwrap().clear();
}

/// Human-readable table, grouped by category, e.g. to return from a
/// debug command/endpoint.
pub fn report() -> String {
    format_report(snapshot())
}

/// Same as [`report`], but restricted to the given categories. An empty
/// slice behaves like [`report`] (everything). If none of the requested
/// categories have any data, says so explicitly instead of printing an
/// empty report.
pub fn report_filtered<S: AsRef<str>>(categories: &[S]) -> String {
    if categories.is_empty() {
        return report();
    }
    let filtered = snapshot_filtered(categories);
    if filtered.is_empty() {
        let names: Vec<&str> = categories.iter().map(|c| c.as_ref()).collect();
        return format!(
            "No timing data recorded for categor{}: {}",
            if names.len() == 1 { "y" } else { "ies" },
            names.join(", ")
        );
    }
    format_report(filtered)
}

fn format_report(categories: Vec<(&'static str, Vec<(&'static str, FnStats)>)>) -> String {
    if categories.is_empty() {
        return "No timing data recorded (is the `timing` feature enabled for this build?)"
            .to_string();
    }

    let mut out = String::new();
    for (category, fns) in categories {
        out.push_str(&format!("== {category} ==\n"));
        out.push_str(&format!(
            "{:<32} {:>8} {:>12} {:>12} {:>12} {:>12} {:>24}\n",
            "function", "calls", "total", "avg", "min", "max", "vs baseline"
        ));
        for (name, s) in fns {
            let drift = match (s.drift_pct(), s.baseline_avg_ns) {
                (Some(pct), Some(baseline)) => {
                    format!("{:+.1}% (was {})", pct, fmt_duration(baseline as u64))
                }
                _ => format!("(<{BASELINE_SAMPLE_SIZE} calls)"),
            };
            out.push_str(&format!(
                "{:<32} {:>8} {:>12} {:>12} {:>12} {:>12} {:>24}\n",
                name,
                s.count,
                fmt_duration(s.total_ns),
                fmt_duration(s.avg_ns() as u64),
                fmt_duration(s.min_ns),
                fmt_duration(s.max_ns),
                drift,
            ));
        }
        out.push('\n');
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
///     "request_parsing", "search_command_parse",
///     SearchCommand::parse(&body, ctx.format)?
/// );
///
/// let hits = core_timing::timed_block!("searching", "shard_search", manager.search(&command).await)?;
/// ```
///
/// The 2-arg form `timed_block!("label", expr)` still works and is
/// grouped under `"uncategorized"`.
///
/// Same zero-cost-when-off behavior as `#[timed]`: requires the *calling*
/// crate to declare its own `timing` feature (see the crate docs / README).
#[macro_export]
macro_rules! timed_block {
    ($label:expr, $body:expr) => {
        $crate::timed_block!("uncategorized", $label, $body)
    };
    ($category:expr, $label:expr, $body:expr) => {{
        if cfg!(feature = "timing") {
            let __perf_start = ::std::time::Instant::now();
            let __perf_result = $body;
            $crate::record($category, $label, __perf_start.elapsed());
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
