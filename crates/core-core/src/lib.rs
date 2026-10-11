pub mod metrics;
mod options;
pub mod reindex;
mod shard_db;
pub mod shard_manager;
pub mod shard_worker;
pub mod shared_state;
use core_timing::timed;
pub use options::DatabaseOptions;
pub use shard_db::ShardDb;
pub mod segment_compaction;
pub mod shard_manager_helpers;

/// Returns the shard that owns `external_id` in a database with `num_shards` shards.
///
/// Where every stored document lives depends on this function, so its output must never
/// change: not across restarts, machines, CPU features, Rust releases or dependency
/// upgrades. That rules out `std`'s `DefaultHasher` (its algorithm is unspecified) and
/// `ahash` (random keys per process, and AES-dependent output per CPU). The function is
/// pinned by the golden values in the tests below; changing its output requires moving
/// every stored document to its new shard first.
///
/// Hash: 64-bit FNV-1a over the UTF-8 bytes, finished with MurmurHash3's `fmix64`.
/// Bucket: jump consistent hash (Lamping & Veach, 2014). It is as uniform as `%`, and if
/// the shard count ever grows from n to n + 1, only about 1/(n + 1) of the documents move.
#[timed(shard_manager)]
pub fn shard_for(external_id: &str, num_shards: u16) -> u16 {
    shard_for_untimed(external_id, num_shards)
}

/// `shard_for` without the timing wrapper, for loops over every stored document, where the
/// `timing` feature would otherwise take its global lock once per id.
pub(crate) fn shard_for_untimed(external_id: &str, num_shards: u16) -> u16 {
    if num_shards <= 1 {
        return 0;
    }
    jump_consistent_hash(stable_hash64(external_id.as_bytes()), num_shards)
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Platform-independent 64-bit hash: FNV-1a followed by the MurmurHash3 finaliser, so every
/// input bit affects every output bit.
pub fn stable_hash64(bytes: &[u8]) -> u64 {
    fmix64(fnv1a64(bytes))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn fmix64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^= value >> 33;
    value
}

/// Lamping & Veach, "A Fast, Minimal Memory, Consistent Hash Algorithm" (2014).
/// Only IEEE-754 operations with defined rounding are used, so the result is the same on
/// every platform Rust supports. `num_buckets` must be at least 1.
fn jump_consistent_hash(mut key: u64, num_buckets: u16) -> u16 {
    let mut bucket: i64 = -1;
    let mut next: i64 = 0;
    while next < i64::from(num_buckets) {
        bucket = next;
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        next = (((bucket + 1) as f64) * ((1u64 << 31) as f64 / (((key >> 33) + 1) as f64))) as i64;
    }
    bucket as u16
}


