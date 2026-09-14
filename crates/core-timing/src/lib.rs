//! Lightweight aggregated function-timing registry.
//!
//! Apply `#[timed(category)]` to any function (sync or async, free fn or
//! method). When the crate's `timing` feature is off, the instrumentation
//! compiles to nothing. When it's on, each call updates an in-memory
//! aggregate (count / total / min / max / baseline) keyed by
//! `(category, function name, source file)` — the file is included so two
//! functions with the same name in different files (e.g. two `load`s)
//! show up as separate rows instead of merging into one. Call [`report`]
//! from a debug command, admin endpoint, or test to see the results,
//! grouped by category.
//!
//! This is intentionally decoupled from `tracing` — it doesn't need a
//! subscriber installed, and it aggregates instead of emitting one
//! event/span per call. Use it alongside `#[tracing::instrument]` if you
//! also want per-call spans in your existing tracing output.
//!
//! ## Byte-rate tracking
//!
//! Raw per-call averages are misleading for batch functions: a call that
//! processes a small batch and one that processes a huge batch at the
//! *same underlying speed* will show wildly different `avg` numbers,
//! because `avg` is per-*call*, not per-*unit-of-work*. Document counts
//! don't fix this either — a batch of tiny documents and a batch of huge
//! documents can have the same doc count but very different amounts of
//! actual work. Bytes processed is the number that's actually comparable
//! across differently-sized batches and differently-sized documents.
//!
//! A function can report how many bytes it processed via [`add_bytes`],
//! called once per invocation, anywhere inside a `#[timed]`-wrapped body,
//! using the same category/name/file the macro generates. This adds an
//! `MB/s`-or-`KB/s` rate column to the report that stays stable
//! regardless of batch size or document size.
//!
//! ```ignore
//! #[timed(inserting)]
//! pub fn insert(&mut self, inputs: Vec<DocumentInput>) -> io::Result<()> {
//!     let bytes: u64 = inputs.iter().map(|d| d.source.len() as u64).sum();
//!     core_timing::add_bytes("inserting", "insert", file!(), bytes);
//!     // ... actual insert work ...
//! }
//! ```
//!
//! Functions that don't report bytes keep behaving exactly as before
//! (rate column shows `-`).
//!
//! ## Baseline semantics
//!
//! The baseline is frozen once a function has accumulated **both** at
//! least [`BASELINE_MIN_CALLS`] calls **and** at least
//! [`BASELINE_MIN_ELAPSED_NS`] of total recorded time — not just call
//! count. A function called millions of times per second would otherwise
//! freeze its baseline a few microseconds into a run, against a cold,
//! unrepresentative sample; gating on elapsed time as well means the
//! baseline reflects some real amount of warmed-up work.
//!
//! For byte-tracked functions, the baseline is a **rate** (bytes/sec) and
//! drift is the percent change in that rate — **positive means faster
//! than baseline**, negative means slower. For functions with no bytes
//! reported, the baseline falls back to average call duration, but drift
//! is still expressed on the same "positive = faster" scale (i.e. it's
//! `(baseline_avg - current_avg) / baseline_avg`), so the sign is
//! consistent everywhere in the report.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub use core_timing_macros::timed;

/// Minimum number of calls before a baseline can be frozen.
pub const BASELINE_MIN_CALLS: u64 = 20;

/// Minimum total recorded time (ns) before a baseline can be frozen, in
/// addition to `BASELINE_MIN_CALLS`. Prevents a high-frequency function
/// from freezing its baseline against a handful of cold-start
/// microseconds. 500ms by default.
pub const BASELINE_MIN_ELAPSED_NS: u64 = 500_000_000;

#[derive(Debug, Default, Clone, Copy)]
pub struct FnStats {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,

    /// Total bytes of work done across all calls (e.g. bytes of document
    /// source data inserted), if the call site reports it via
    /// [`add_bytes`]. Zero if never reported.
    pub total_bytes: u64,

    /// Average call duration (ns) as of the point the baseline froze.
    /// Used as the baseline when no bytes are tracked. `None` until the
    /// baseline conditions are met.
    pub baseline_avg_ns: Option<f64>,
    /// Throughput (bytes/sec) as of the point the baseline froze. Used as
    /// the baseline when bytes *are* tracked. `None` until the baseline
    /// conditions are met, or if no bytes were ever reported.
    pub baseline_bytes_rate: Option<f64>,
}

impl FnStats {
    fn empty() -> Self {
        FnStats {
            min_ns: u64::MAX,
            ..Default::default()
        }
    }

    pub fn avg_ns(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.count as f64
        }
    }

    /// Current throughput in bytes/sec, if this function has ever
    /// reported bytes. `None` if no bytes were reported or no time has
    /// elapsed yet.
    pub fn bytes_per_sec(&self) -> Option<f64> {
        if self.total_bytes == 0 || self.total_ns == 0 {
            return None;
        }
        Some(self.total_bytes as f64 / (self.total_ns as f64 / 1_000_000_000.0))
    }

    /// Percent change vs. the frozen baseline, on a "positive = faster"
    /// scale regardless of whether the comparison is rate-based or
    /// duration-based under the hood. `None` if there's no baseline yet.
    pub fn drift_pct(&self) -> Option<f64> {
        if let Some(baseline_rate) = self.baseline_bytes_rate {
            let current_rate = self.bytes_per_sec()?;
            if baseline_rate <= 0.0 {
                return None;
            }
            return Some((current_rate - baseline_rate) / baseline_rate * 100.0);
        }
        let baseline_avg = self.baseline_avg_ns?;
        if baseline_avg <= 0.0 {
            return None;
        }
        let current_avg = self.avg_ns();
        Some((baseline_avg - current_avg) / baseline_avg * 100.0)
    }
}

// (category, function name, source filename — no directories)
type Key = (&'static str, &'static str, &'static str);

fn registry() -> &'static Mutex<HashMap<Key, FnStats>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Key, FnStats>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Strips a `file!()`-style path (e.g. `crates/core-core/src/shard_db.rs`,
/// or `src\shard_db.rs` on Windows) down to just the filename. Slicing a
/// `&'static str` yields another `&'static str`, so no allocation.
fn filename_only(file: &'static str) -> &'static str {
    file.rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(file)
}

/// Called by the `#[timed]` macro / `timed_block!`. Not usually called
/// directly. `file` is expected to be the output of the builtin `file!()`
/// macro at the call site — full path in, only the filename is kept.
///
/// If the same call already reported bytes via [`add_bytes`] before
/// returning, that's folded into this call's baseline once both baseline
/// conditions (`BASELINE_MIN_CALLS`, `BASELINE_MIN_ELAPSED_NS`) are met —
/// call `add_bytes` *before* the function returns (i.e. anywhere in the
/// body) so it's visible here.
///
/// Note: this takes a plain `Mutex<HashMap<..>>` lock per call, which is
/// fine for dev-time profiling. If you end up applying `#[timed]` to a
/// function called millions of times per second across many shard
/// threads and see contention, prefer timing the boundary functions
/// (parse, index-write, disk-write) rather than every inner helper.
pub fn record(category: &'static str, name: &'static str, file: &'static str, elapsed: Duration) {
    let file = filename_only(file);
    let ns = elapsed.as_nanos() as u64;
    let mut map = registry().lock().unwrap();
    let entry = map.entry((category, name, file)).or_insert_with(FnStats::empty);
    entry.count += 1;
    entry.total_ns += ns;
    if ns < entry.min_ns {
        entry.min_ns = ns;
    }
    if ns > entry.max_ns {
        entry.max_ns = ns;
    }
    if entry.baseline_avg_ns.is_none()
        && entry.count >= BASELINE_MIN_CALLS
        && entry.total_ns >= BASELINE_MIN_ELAPSED_NS
    {
        entry.baseline_avg_ns = Some(entry.avg_ns());
        if entry.total_bytes > 0 {
            entry.baseline_bytes_rate = entry.bytes_per_sec();
        }
    }
}

/// Reports how many bytes of work a `#[timed]`-wrapped function did on
/// top of just how long it took, so the report can show a stable
/// `MB/s`/`KB/s` rate instead of a per-call average that's skewed by
/// batch or document size.
///
/// Call this once per invocation, anywhere inside the function body,
/// using the *same* `category`/`name`/`file` the `#[timed(category)]`
/// attribute on that function would generate (`file` should just be
/// `file!()` at the call site — same as the macro uses). If a function is
/// called from multiple call sites with different labels, make sure the
/// label passed here matches whichever one is actually active.
///
/// If a call errors out or only partially completes its work, report the
/// bytes actually processed (e.g. only the documents that succeeded),
/// not the bytes requested — otherwise the rate is optimistic. Be
/// consistent about this across call sites that feed into the same
/// category, so `MB/s` means the same thing on every row.
pub fn add_bytes(category: &'static str, name: &'static str, file: &'static str, bytes: u64) {
    let file = filename_only(file);
    let mut map = registry().lock().unwrap();
    let entry = map.entry((category, name, file)).or_insert_with(FnStats::empty);
    entry.total_bytes += bytes;
}

type CategoryEntries = Vec<(&'static str, &'static str, FnStats)>; // (name, file, stats)

/// Snapshot grouped by category, each category's entries sorted by total
/// time descending, categories themselves sorted by their summed total
/// time descending (biggest time-sink category first).
pub fn snapshot() -> Vec<(&'static str, CategoryEntries)> {
    let map = registry().lock().unwrap();
    let mut by_category: HashMap<&'static str, CategoryEntries> = HashMap::new();
    for (&(category, name, file), &stats) in map.iter() {
        by_category
            .entry(category)
            .or_default()
            .push((name, file, stats));
    }

    let mut categories: Vec<(&'static str, CategoryEntries)> = by_category.into_iter().collect();
    for (_, entries) in categories.iter_mut() {
        entries.sort_by(|a, b| b.2.total_ns.cmp(&a.2.total_ns));
    }
    categories.sort_by(|a, b| {
        let a_total: u64 = a.1.iter().map(|(_, _, s)| s.total_ns).sum();
        let b_total: u64 = b.1.iter().map(|(_, _, s)| s.total_ns).sum();
        b_total.cmp(&a_total)
    });
    categories
}

/// Same as [`snapshot`], but restricted to the given categories and/or a
/// specific source file (exact filename match, e.g. `"shard_db.rs"` — not
/// a path). An empty `categories` slice means "any category"; `file =
/// None` means "any file". Passing both narrows to their intersection.
pub fn snapshot_filtered<S: AsRef<str>>(
    categories: &[S],
    file: Option<&str>,
) -> Vec<(&'static str, CategoryEntries)> {
    let by_category = if categories.is_empty() {
        snapshot()
    } else {
        snapshot()
            .into_iter()
            .filter(|(category, _)| categories.iter().any(|c| c.as_ref() == *category))
            .collect()
    };

    let Some(file) = file else {
        return by_category;
    };
    by_category
        .into_iter()
        .map(|(category, entries)| {
            let entries: CategoryEntries = entries
                .into_iter()
                .filter(|(_, entry_file, _)| *entry_file == file)
                .collect();
            (category, entries)
        })
        .filter(|(_, entries)| !entries.is_empty())
        .collect()
}

/// Clear all recorded stats (including baselines and byte counters) —
/// useful between benchmark runs.
pub fn reset() {
    registry().lock().unwrap().clear();
}

/// Human-readable table, grouped by category, e.g. to return from a
/// debug command/endpoint. Each row is labeled `name (file.rs)` so
/// same-named functions in different files don't collide.
pub fn report() -> String {
    format_report(snapshot())
}

/// Same as [`report`], but restricted to the given categories and/or a
/// specific source file — see [`snapshot_filtered`] for exact matching
/// rules. Empty categories + `None` file behaves like [`report`]. If the
/// filters match nothing, says so explicitly instead of printing an
/// empty report.
pub fn report_filtered<S: AsRef<str>>(categories: &[S], file: Option<&str>) -> String {
    if categories.is_empty() && file.is_none() {
        return report();
    }
    let filtered = snapshot_filtered(categories, file);
    if filtered.is_empty() {
        let mut parts = Vec::new();
        if !categories.is_empty() {
            let names: Vec<&str> = categories.iter().map(|c| c.as_ref()).collect();
            parts.push(format!(
                "categor{} {}",
                if names.len() == 1 { "y" } else { "ies" },
                names.join(", ")
            ));
        }
        if let Some(f) = file {
            parts.push(format!("file {f}"));
        }
        return format!("No timing data recorded for {}", parts.join(" and "));
    }
    format_report(filtered)
}

fn format_report(categories: Vec<(&'static str, CategoryEntries)>) -> String {
    if categories.is_empty() {
        return "No timing data recorded (is the `timing` feature enabled for this build?)"
            .to_string();
    }

    let mut out = String::new();
    for (category, entries) in categories {
        // Drop orphaned entries: created by an add_bytes() call whose
        // (category, name, file) never matched a #[timed] call, so
        // count stays 0 and min_ns is stuck at its u64::MAX sentinel.
        // These carry no timing data and would print garbage (a bogus
        // multi-billion-second "max").
        let entries: CategoryEntries = entries.into_iter().filter(|(_, _, s)| s.count > 0).collect();
        if entries.is_empty() {
            continue;
        }

        out.push_str(&format!("== {category} ==\n"));

        // Column widths sized to this category's actual content, so a
        // long function or file name widens the column instead of
        // wrecking alignment for the whole table.
        let name_width = entries
            .iter()
            .map(|(name, _, _)| name.len())
            .max()
            .unwrap_or(0)
            .max("function".len());
        let file_width = entries
            .iter()
            .map(|(_, file, _)| file.len())
            .max()
            .unwrap_or(0)
            .max("file".len());

        out.push_str(&format!(
            "{:<name_width$}  {:<file_width$} {:>8} {:>12} {:>12} {:>12} {:>12} {:>14} {:>24}\n",
            "function", "file", "calls", "total", "avg", "min", "max", "rate", "vs baseline",
        ));
        for (name, file, s) in entries {
            let rate = match s.bytes_per_sec() {
                Some(bps) => fmt_bytes_rate(bps),
                None => "-".to_string(),
            };
            let drift = match s.drift_pct() {
                Some(pct) if s.baseline_bytes_rate.is_some() => {
                    format!("{:+.1}% (was {})", pct, fmt_bytes_rate(s.baseline_bytes_rate.unwrap()))
                }
                Some(pct) => {
                    format!("{:+.1}% (was {})", pct, fmt_duration(s.baseline_avg_ns.unwrap() as u64))
                }
                None => "(warming up)".to_string(),
            };
            out.push_str(&format!(
                "{:<name_width$}  {:<file_width$} {:>8} {:>12} {:>12} {:>12} {:>12} {:>14} {:>24}\n",
                name,
                file,
                s.count,
                fmt_duration(s.total_ns),
                fmt_duration(s.avg_ns() as u64),
                fmt_duration(s.min_ns),
                fmt_duration(s.max_ns),
                rate,
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
/// The source file is captured automatically via `file!()` at the call
/// site — nothing to pass for that part.
#[macro_export]
macro_rules! timed_block {
    ($label:expr, $body:expr) => {
        $crate::timed_block!("uncategorized", $label, $body)
    };
    ($category:expr, $label:expr, $body:expr) => {{
        if cfg!(feature = "timing") {
            let __perf_start = ::std::time::Instant::now();
            let __perf_result = $body;
            $crate::record($category, $label, file!(), __perf_start.elapsed());
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

fn fmt_bytes_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_048_576.0 {
        format!("{:.1} MB/s", bytes_per_sec / 1_048_576.0)
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{bytes_per_sec:.0} B/s")
    }
}