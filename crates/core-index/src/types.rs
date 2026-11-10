// Keep track of all types, will be needed for binary in the future
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────
// Primitive aliases
// ─────────────────────────────────────────────────────────────

pub type DocId = u64;
pub type LocalDocId = u64;
pub type Position = u32;
pub type XPathId = u32;

// ─────────────────────────────────────────────────────────────
// Sharding
// ─────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────
// Document ID packing
//   layout: [63: array-row flag][62..48: shard][47..0: local id]
// ─────────────────────────────────────────────────────────────

pub const SHARD_BITS: u32 = 15;
pub const LOCAL_BITS: u32 = 48;

/// Largest shard-local id that fits without overflowing into the shard bits.
pub const MAX_LOCAL_DOC_ID: LocalDocId = (1u64 << LOCAL_BITS) - 1;
pub const MAX_SHARD_ID: u16 = (1u16 << SHARD_BITS) - 1;

pub fn make_doc_id(shard: ShardId, local: LocalDocId) -> DocId {
    assert!(
        local <= MAX_LOCAL_DOC_ID,
        "local document ID {local} exceeds the {LOCAL_BITS}-bit limit"
    );
    assert!(
        shard.0 <= MAX_SHARD_ID,
        "shard id {shard} exceeds the 15-bit limit (bit 63 reserved for array rows)"
    );
    ((shard.0 as u64) << LOCAL_BITS) | local
}

/// Shard that owns a globally packed document ID.
pub fn shard_of(doc: DocId) -> ShardId {
    ShardId(((doc >> LOCAL_BITS) & u64::from(MAX_SHARD_ID)) as u16)
}

/// Shard-local id contained in a globally packed document ID.
pub fn local_of(doc: DocId) -> LocalDocId {
    doc & MAX_LOCAL_DOC_ID
}

// ─────────────────────────────────────────────────────────────
// Array rows
// ─────────────────────────────────────────────────────────────

/// Bit 63 marks an array-row id, so a row id can never equal a document id.
pub const ARRAY_ROW_FLAG: u64 = 1 << 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ArrayRowId(pub u64);

impl ArrayRowId {
    /// Raw u64 as stored in a posting / on disk (bit 63 always set).
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Re-wrap a raw id read from a posting or the index.
    pub const fn from_raw(raw: u64) -> Self {
        ArrayRowId(raw)
    }

    /// Per-shard sequence number (flag stripped).
    pub const fn seq(self) -> u64 {
        self.0 & !ARRAY_ROW_FLAG
    }
}

/// Bits of a stored position that hold how many positions its token covers.
pub const SPAN_BITS: u32 = 4;
/// Most positions one token can cover ("spider-man" covers 2: spider, man).
pub const MAX_SPAN: u32 = 1 << SPAN_BITS;

/// Packs a token's start and span so sorting stored positions still sorts by
/// start. Starts past what fits are capped.
pub fn encode_position(start: u32, span: u32) -> Position {
    (start.min(u32::MAX >> SPAN_BITS) << SPAN_BITS) | (span.clamp(1, MAX_SPAN) - 1)
}

pub fn position_start(position: Position) -> u32 {
    position >> SPAN_BITS
}

pub fn position_span(position: Position) -> u32 {
    (position & (MAX_SPAN - 1)) + 1
}

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

// Cached entry of the field stats, once computed in the index, will store smaller information.
// TODO: Persist this on disk; small size increase, decent performance benefit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldStats {
    pub doc_count: u64,
    pub total_doc_len: u64,
}

impl FieldStats {
    pub fn add(&mut self, other: &FieldStats) {
        self.doc_count += other.doc_count;
        self.total_doc_len += other.total_doc_len;
    }
}
