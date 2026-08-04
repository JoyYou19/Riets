use std::time::{Duration, Instant};

pub struct RequestTimer {
    started: Instant,
}
impl RequestTimer {
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
        }
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

//WARN: in database.rs i use this with mutex to avoid &mut self, might need improvement idk
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseMetrics {
    pub search_requests: u64,
    pub search_errors: u64,
    pub search_total_time: Duration,
    pub indexing_requests: u64,
    pub indexing_errors: u64,
    pub indexing_total_time: Duration,
    pub reindex_requests: u64,
    pub reindex_errors: u64,
    pub reindex_total_time: Duration,
}
impl DatabaseMetrics {
    pub fn average_search_time(&self) -> Option<Duration> {
        if self.search_requests == 0 {
            return None;
        }
        Some(self.search_total_time / self.search_requests as u32)
    }
}
