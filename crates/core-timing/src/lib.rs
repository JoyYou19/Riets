//! Lightweight aggregated function-timing registry.
//!
//! Apply `#[timed(category)]` to any function (sync or async, free fn or
//! method), or wrap an expression in [`timed_block!`]. Each call updates an
//! in-memory aggregate (count / total / min / max / baseline) keyed by
//! `(category, function name, source file)`. Call [`report`] from a debug
//! command, admin endpoint, or test to see the results, grouped by category.
//!
//! ## One switch: the `timing` feature of *this* crate
//!
//! * **Feature off (default):** nothing is recorded and nothing is
//!   compiled that could cost anything at runtime. There is no registry,
//!   no `Mutex`, no `HashMap`, no `Instant::now()`. [`Timer`] is a
//!   zero-sized type, [`record`] and [`add_bytes`] are empty inline
//!   functions, and [`report`] says that timing is disabled.
//! * **Feature on:** every call is aggregated into a registry that is split
//!   into several independently locked shards, so threads working on
//!   different functions rarely contend on the same lock.
//!
//! Consumers just depend on `core-timing` and forward the feature if they
//! want a flag of their own:
//!
//! ```toml
//! [features]
//! timing = ["core-timing/timing"]
//! ```
//!
//! They do **not** need to declare a feature for the macros to work: the
//! macros always expand to code that calls into this crate, and this crate
//! decides whether that code does anything. Because Cargo unifies features,
//! enabling `core-timing/timing` anywhere in the build turns timing on
//! everywhere, consistently.
//!
//! This is intentionally decoupled from `tracing`: it needs no subscriber
//! and it aggregates instead of emitting one event per call.
//!
//! ## Byte-rate tracking
//!
//! Raw per-call averages are misleading for batch functions: a small batch
//! and a huge batch processed at the same speed show very different `avg`
//! numbers. Bytes processed is comparable across batch sizes.
//!
//! A function can report how many bytes it processed with the
//! [`add_bytes!`] macro, once per invocation, anywhere inside a
//! `#[timed]`-wrapped body, using the same category and label the macro
//! uses. That adds a `MB/s` / `KB/s` rate column to the report.
//!
//! ```ignore
//! #[timed(inserting)]
//! pub fn insert(&mut self, inputs: Vec<DocumentInput>) -> io::Result<()> {
//!     // The byte-counting expression is NOT evaluated when timing is off.
//!     core_timing::add_bytes!(
//!         "inserting",
//!         "insert",
//!         inputs.iter().map(|d| d.source.len() as u64).sum::<u64>()
//!     );
//!     // ... actual insert work ...
//! }
//! ```
//!
//! Functions that don't report bytes show `-` in the rate column.
//!
//! ## Baseline semantics
//!
//! The baseline is frozen once a function has accumulated **both** at least
//! [`BASELINE_MIN_CALLS`] calls **and** at least [`BASELINE_MIN_ELAPSED_NS`]
//! of total recorded time. A function called millions of times per second
//! would otherwise freeze its baseline against a cold, unrepresentative
//! sample.
//!
//! For byte-tracked functions the baseline is a **rate** (bytes/sec) and
//! drift is the percent change in that rate. For functions with no bytes
//! reported, the baseline is the average call duration. In both cases drift
//! is on a "positive = faster" scale.

pub use core_timing_macros::timed;

/// `true` when this crate's `timing` feature is enabled.
///
/// The macros branch on this constant. It is a `const`, so the compiler
/// removes the dead branch, including the arguments of [`add_bytes!`].
pub const ENABLED: bool = cfg!(feature = "timing");

/// Minimum number of calls before a baseline can be frozen.
pub const BASELINE_MIN_CALLS: u64 = 20;

/// Minimum total recorded time (ns) before a baseline can be frozen, in
/// addition to [`BASELINE_MIN_CALLS`]. 500ms by default.
pub const BASELINE_MIN_ELAPSED_NS: u64 = 500_000_000;

/// Text returned by the report functions when there is nothing to show.
const NOTHING_RECORDED: &str =
    "No timing data recorded (is the `timing` feature of `core-timing` enabled for this build?)";

#[derive(Debug, Default, Clone, Copy)]
pub struct FnStats {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,

    /// Total bytes of work done across all calls, if the call site reports
    /// it via [`add_bytes!`]. Zero if never reported.
    pub total_bytes: u64,

    /// Average call duration (ns) as of the point the baseline froze.
    /// `None` until the baseline conditions are met.
    pub baseline_avg_ns: Option<f64>,
    /// Throughput (bytes/sec) as of the point the baseline froze.
    /// `None` until the baseline conditions are met, or if no bytes were
    /// ever reported.
    pub baseline_bytes_rate: Option<f64>,
}

impl FnStats {
    pub fn avg_ns(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.count as f64
        }
    }

    /// Current throughput in bytes/sec, if this function has ever reported
    /// bytes. `None` if no bytes were reported or no time has elapsed yet.
    pub fn bytes_per_sec(&self) -> Option<f64> {
        if self.total_bytes == 0 || self.total_ns == 0 {
            return None;
        }
        Some(self.total_bytes as f64 / (self.total_ns as f64 / 1_000_000_000.0))
    }

    /// Percent change vs. the frozen baseline, on a "positive = faster"
    /// scale regardless of whether the comparison is rate-based or
    /// duration-based. `None` if there's no baseline yet.
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

/// `(function name, source filename, stats)` entries of one category.
pub type CategoryEntries = Vec<(&'static str, &'static str, FnStats)>;

// ---------------------------------------------------------------------------
// Feature ON: the real implementation.
// ---------------------------------------------------------------------------

#[cfg(feature = "timing")]
mod enabled {
    use super::{
        BASELINE_MIN_CALLS, BASELINE_MIN_ELAPSED_NS, CategoryEntries, FnStats, NOTHING_RECORDED,
    };
    use std::collections::HashMap;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    // (category, function name, source filename, no directories)
    type Key = (&'static str, &'static str, &'static str);
    type Shard = Mutex<HashMap<Key, FnStats>>;

    /// Number of independently locked registry shards. A key always maps to
    /// the same shard, so per-key updates stay consistent while unrelated
    /// functions do not fight over one global lock.
    const SHARDS: usize = 16;

    impl FnStats {
        fn empty() -> Self {
            FnStats {
                min_ns: u64::MAX,
                ..Default::default()
            }
        }
    }

    fn shards() -> &'static [Shard; SHARDS] {
        static SHARDS_CELL: OnceLock<[Shard; SHARDS]> = OnceLock::new();
        SHARDS_CELL.get_or_init(|| std::array::from_fn(|_| Mutex::new(HashMap::new())))
    }

    /// Locks the shard owning `key`. A panic while a lock was held cannot
    /// leave the map invalid (each update is a few plain field writes), so a
    /// poisoned lock is taken over instead of turning every later timed call
    /// into a panic.
    fn lock_shard(key: &Key) -> MutexGuard<'static, HashMap<Key, FnStats>> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let index = (hasher.finish() as usize) % SHARDS;
        shards()[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Drop guard created by `#[timed]`: records the time from
    /// [`Timer::start`] to drop. Not meant to be used directly.
    #[doc(hidden)]
    #[must_use = "the timer records when it is dropped; bind it to a variable"]
    pub struct Timer {
        category: &'static str,
        name: &'static str,
        file: &'static str,
        start: Instant,
    }

    impl Timer {
        #[inline]
        pub fn start(category: &'static str, name: &'static str, file: &'static str) -> Self {
            Timer {
                category,
                name,
                file,
                start: Instant::now(),
            }
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            // a call cut short by a panic did not do its normal work; it
            // would skew the stats
            if std::thread::panicking() {
                return;
            }
            record(self.category, self.name, self.file, self.start.elapsed());
        }
    }

    /// Strips a `file!()`-style path (`crates/x/src/a.rs` or `src\a.rs`)
    /// down to the filename. Slicing a `&'static str` yields another
    /// `&'static str`, so no allocation.
    fn filename_only(file: &'static str) -> &'static str {
        file.rsplit(['/', '\\']).next().unwrap_or(file)
    }

    /// Records one call. Called by `#[timed]` / `timed_block!`; not usually
    /// called directly. `file` is the output of `file!()` at the call site.
    ///
    /// Bytes reported via `add_bytes!` before the call returns are folded
    /// into the baseline once both baseline conditions are met.
    pub fn record(
        category: &'static str,
        name: &'static str,
        file: &'static str,
        elapsed: Duration,
    ) {
        let key = (category, name, filename_only(file));
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let mut map = lock_shard(&key);
        let entry = map.entry(key).or_insert_with(FnStats::empty);
        entry.count = entry.count.saturating_add(1);
        entry.total_ns = entry.total_ns.saturating_add(ns);
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

    /// Reports how many bytes of work a timed function did. Prefer the
    /// `add_bytes!` macro, which also skips evaluating `bytes` when timing
    /// is disabled.
    ///
    /// Use the same `category` / `name` the `#[timed]` attribute generates
    /// and `file!()` for `file`. If a call errors out or completes only
    /// partially, report the bytes actually processed, not the bytes
    /// requested, so the rate is not optimistic.
    pub fn add_bytes(category: &'static str, name: &'static str, file: &'static str, bytes: u64) {
        let key = (category, name, filename_only(file));
        let mut map = lock_shard(&key);
        let entry = map.entry(key).or_insert_with(FnStats::empty);
        entry.total_bytes = entry.total_bytes.saturating_add(bytes);
    }

    /// Snapshot grouped by category, each category's entries sorted by total
    /// time descending, categories sorted by their summed total time
    /// descending.
    pub fn snapshot() -> Vec<(&'static str, CategoryEntries)> {
        let mut by_category: HashMap<&'static str, CategoryEntries> = HashMap::new();
        // one shard at a time, so a snapshot never blocks all writers at once
        for shard in shards() {
            let map = shard.lock().unwrap_or_else(PoisonError::into_inner);
            for (&(category, name, file), &stats) in map.iter() {
                by_category
                    .entry(category)
                    .or_default()
                    .push((name, file, stats));
            }
        }

        let mut categories: Vec<(&'static str, CategoryEntries)> =
            by_category.into_iter().collect();
        for (_, entries) in &mut categories {
            entries.sort_by_key(|e| std::cmp::Reverse(e.2.total_ns));
        }
        categories.sort_by(|a, b| {
            let a_total: u64 = a.1.iter().map(|(_, _, s)| s.total_ns).sum();
            let b_total: u64 = b.1.iter().map(|(_, _, s)| s.total_ns).sum();
            b_total.cmp(&a_total)
        });
        categories
    }

    /// Same as [`snapshot`], restricted to the given categories and/or an
    /// exact source filename (e.g. `"shard_db.rs"`, not a path). An empty
    /// `categories` slice means any category; `file = None` means any file.
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

    /// Clear all recorded stats (including baselines and byte counters),
    /// useful between benchmark runs.
    pub fn reset() {
        for shard in shards() {
            shard.lock().unwrap_or_else(PoisonError::into_inner).clear();
        }
    }

    /// Human-readable table, grouped by category. Each row is labeled with
    /// the function and its file so same-named functions don't collide.
    pub fn report() -> String {
        format_report(snapshot())
    }

    /// Same as [`report`], restricted to the given categories and/or a
    /// source file; see [`snapshot_filtered`]. If the filters match nothing,
    /// says so explicitly.
    pub fn report_filtered<S: AsRef<str>>(categories: &[S], file: Option<&str>) -> String {
        if categories.is_empty() && file.is_none() {
            return report();
        }
        let filtered = snapshot_filtered(categories, file);
        if filtered.is_empty() {
            let mut parts = Vec::new();
            if !categories.is_empty() {
                let names: Vec<&str> = categories.iter().map(AsRef::as_ref).collect();
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
            return NOTHING_RECORDED.to_string();
        }

        let mut out = String::new();
        for (category, entries) in categories {
            // Drop orphaned entries created by an add_bytes() call whose key
            // never matched a timed call: count stays 0 and min_ns is stuck
            // at its u64::MAX sentinel, which would print garbage.
            let entries: CategoryEntries = entries
                .into_iter()
                .filter(|(_, _, s)| s.count > 0)
                .collect();
            if entries.is_empty() {
                continue;
            }

            out.push_str(&format!("== {category} ==\n"));

            // Column widths sized to this category's content.
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
                let drift = match (s.drift_pct(), s.baseline_bytes_rate, s.baseline_avg_ns) {
                    (Some(pct), Some(rate), _) => {
                        format!("{:+.1}% (was {})", pct, fmt_bytes_rate(rate))
                    }
                    (Some(pct), None, Some(avg)) => {
                        format!("{:+.1}% (was {})", pct, fmt_duration(avg as u64))
                    }
                    _ => "(warming up)".to_string(),
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
        if out.is_empty() {
            return NOTHING_RECORDED.to_string();
        }
        out
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
}

#[cfg(feature = "timing")]
pub use enabled::*;

// ---------------------------------------------------------------------------
// Feature OFF: same public API, every function is an empty inline stub.
// No registry, no Mutex, no HashMap, no clock reads.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "timing"))]
mod disabled {
    use super::{CategoryEntries, NOTHING_RECORDED};
    use std::time::Duration;

    /// Zero-sized stand-in for the real timer guard. It has no `Drop`
    /// impl and holds no data, so `#[timed]` costs nothing.
    #[doc(hidden)]
    #[must_use = "the timer records when it is dropped; bind it to a variable"]
    pub struct Timer;

    impl Timer {
        #[inline(always)]
        pub const fn start(
            _category: &'static str,
            _name: &'static str,
            _file: &'static str,
        ) -> Self {
            Timer
        }
    }

    #[inline(always)]
    pub fn record(
        _category: &'static str,
        _name: &'static str,
        _file: &'static str,
        _elapsed: Duration,
    ) {
    }

    #[inline(always)]
    pub fn add_bytes(
        _category: &'static str,
        _name: &'static str,
        _file: &'static str,
        _bytes: u64,
    ) {
    }

    #[inline(always)]
    pub fn snapshot() -> Vec<(&'static str, CategoryEntries)> {
        Vec::new()
    }

    #[inline(always)]
    pub fn snapshot_filtered<S: AsRef<str>>(
        _categories: &[S],
        _file: Option<&str>,
    ) -> Vec<(&'static str, CategoryEntries)> {
        Vec::new()
    }

    #[inline(always)]
    pub fn reset() {}

    pub fn report() -> String {
        NOTHING_RECORDED.to_string()
    }

    pub fn report_filtered<S: AsRef<str>>(_categories: &[S], _file: Option<&str>) -> String {
        NOTHING_RECORDED.to_string()
    }
}

#[cfg(not(feature = "timing"))]
pub use disabled::*;

/// Time an arbitrary expression without needing to own its definition:
/// a function in a dependency you can't annotate, a specific `.await`, a
/// specific loop. Works for sync and async expressions, and `?` / early
/// return inside the expression is fine (the time is still recorded).
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
/// The 2-arg form `timed_block!("label", expr)` is grouped under
/// `"uncategorized"`.
///
/// With the `timing` feature of `core-timing` off, the guard is a
/// zero-sized value and the macro is just `expr`.
#[macro_export]
macro_rules! timed_block {
    ($label:expr, $body:expr) => {
        $crate::timed_block!("uncategorized", $label, $body)
    };
    ($category:expr, $label:expr, $body:expr) => {{
        let __core_timing_guard = $crate::Timer::start($category, $label, ::core::file!());
        $body
    }};
}

/// Report how many bytes of work the surrounding timed function did.
///
/// ```ignore
/// core_timing::add_bytes!("inserting", "insert", inputs.iter().map(|d| d.source.len() as u64).sum::<u64>());
/// ```
///
/// Unlike calling [`add_bytes`] directly, the `$bytes` expression is **not
/// evaluated at all** when the `timing` feature is off, so an expensive
/// byte-counting loop costs nothing in normal builds.
#[macro_export]
macro_rules! add_bytes {
    ($category:expr, $name:expr, $bytes:expr) => {
        if $crate::ENABLED {
            $crate::add_bytes($category, $name, ::core::file!(), $bytes);
        }
    };
}

