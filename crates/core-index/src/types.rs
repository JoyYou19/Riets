// Keep track of all types, will be needed for binary in the future
pub type DocId = u64;
pub type Position = u32;
pub type XPathId = u32;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermKey {
    pub xpath: XPathId,
    pub term: String,
}

impl TermKey {
    pub fn new(term: impl Into<String>, xpath: XPathId) -> Self {
        Self {
            xpath,
            term: term.into(),
        }
    }
}

//TODO: vajag testus prieks taa kaa uztaisaas shardID, kaa tas tiek assigned convertots uz
//external+internal.....

// Cached entry of the field stats, once computed in the index, will store smaller information
// TODO: Need to persist this on the disk probably, could be a good small size increase, but decent
// performance benefit
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldStats {
    pub doc_count: u64,
    pub total_doc_len: u64,
}

// Sharding IDs
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub struct ShardId(pub u16);
impl std::fmt::Display for ShardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl slog::Value for ShardId {
    fn serialize(
        &self,
        _record: &slog::Record,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_u16(key, self.0)
    }
}

impl From<u16> for ShardId {
    fn from(v: u16) -> Self {
        ShardId(v)
    }
}
impl From<ShardId> for u16 {
    fn from(s: ShardId) -> Self {
        s.0
    }
}
impl TryFrom<usize> for ShardId {
    type Error = std::num::TryFromIntError;
    fn try_from(v: usize) -> Result<Self, Self::Error> {
        Ok(ShardId(u16::try_from(v)?))
    }
}
// Identifies the shard that owns the document

pub type LocalDocId = u64;

pub const SHARD_BITS: u32 = 16;
pub const LOCAL_BITS: u32 = 48;

// Maximum sequence that can be repeated LOCALLY without overwriting the shard portion of the
// packed document ID
pub const MAX_LOCAL_DOC_ID: LocalDocId = (1u64 << LOCAL_BITS) - 1;

pub fn make_doc_id(shard: ShardId, local: LocalDocId) -> DocId {
    assert!(
        local <= MAX_LOCAL_DOC_ID,
        "local document ID {local} exceeds the {LOCAL_BITS}-bit limit"
    );
    ((shard.0 as u64) << LOCAL_BITS) | local
}

// Returns the shard that owns a globally packed document ID
pub fn shard_of(doc: DocId) -> ShardId {
    ShardId((doc >> LOCAL_BITS) as u16)
}

// Returns the shard-local id contained in a globally packed document ID
pub fn local_of(doc: DocId) -> LocalDocId {
    doc & ((1u64 << LOCAL_BITS) - 1)
}
