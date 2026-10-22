//! The process-wide pool for long background work: segment writes and compaction. Shared by
//! every shard, so concurrent background work never exceeds the cores, however many shards
//! there are. 
use std::sync::OnceLock;

use rayon::{ ThreadPool, ThreadPoolBuilder };

static POOL: OnceLock<ThreadPool> = OnceLock::new();

/// The shared background pool, one thread per core.
pub fn background_pool() -> &'static ThreadPool {
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("background-{i}"))
            .build()
            .expect("failed to build the background pool")
    })
}