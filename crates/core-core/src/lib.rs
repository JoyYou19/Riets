pub mod metrics;
mod options;
pub mod reindex;
mod shard_db;
pub mod shard_manager;
pub mod shard_worker;
pub mod shared_state;
pub use options::DatabaseOptions;
pub use shard_db::ShardDb;

pub fn shard_for(external_id: &str, num_shards: u16) -> u16 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    //INFO: if there is a single shard it will always go to it
    if num_shards == 0 {
        return 0;
    }

    let mut hasher = DefaultHasher::new();
    external_id.hash(&mut hasher);
    //The math of seperation type beat
    (hasher.finish() % num_shards as u64) as u16
}

//TESTS for asigning something to a shard
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shard_for_deterministic() {
        // Same input always returns same shard
        let shard1 = shard_for("user:123", 4);
        let shard2 = shard_for("user:123", 4);
        assert_eq!(shard1, shard2);
    }

    #[test]
    fn test_shard_for_bounded() {
        // Result is always in valid range
        for id in 0..100 {
            let shard = shard_for(&format!("id:{}", id), 16);
            assert!(shard < 16);
        }
    }

    #[test]
    fn test_shard_for_different_inputs() {
        // Different inputs can route to different shards
        let shards: std::collections::HashSet<_> = (0..100)
            .map(|i| shard_for(&format!("id:{}", i), 16))
            .collect();
        // Rough check: should use multiple shards (not all the same)
        assert!(shards.len() > 1);
    }
}
