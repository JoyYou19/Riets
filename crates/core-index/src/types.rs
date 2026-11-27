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
        _record: &slog::Record<'_>,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_u16(key, self.0)
    }
}

impl From<u16> for ShardId {
    fn from(v: u16) -> Self {
        Self(v)
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
        Ok(Self(u16::try_from(v)?))
    }
}

// ─────────────────────────────────────────────────────────────
// Document ID packing
//   layout: [63: array-row flag][62..54: shard][53..0: local id]
// ─────────────────────────────────────────────────────────────

pub const SHARD_BITS: u32 = 9;
pub const LOCAL_BITS: u32 = 54;

// no gap and no overlap: shard + local fill bits 0..62, bit 63 is the array-row flag
const _: () = assert!(SHARD_BITS + LOCAL_BITS == 63);

/// Largest shard-local id that fits without overflowing into the shard bits.
pub const MAX_LOCAL_DOC_ID: LocalDocId = (1u64 << LOCAL_BITS) - 1;
pub const MAX_SHARD_ID: u16 = (1u16 << SHARD_BITS) - 1;

pub const MAX_SHARDS: u16 = MAX_SHARD_ID + 1;

pub fn make_doc_id(shard: ShardId, local: LocalDocId) -> DocId {
    assert!(
        local <= MAX_LOCAL_DOC_ID,
        "local document ID {local} exceeds the {LOCAL_BITS}-bit limit"
    );
    assert!(
        shard.0 <= MAX_SHARD_ID,
        "shard id {shard} exceeds the {SHARD_BITS}-bit limit (bit 63 reserved for array rows)"
    );
    (u64::from(shard.0) << LOCAL_BITS) | local
}

/// Shard that owns a globally packed document ID.
pub fn shard_of(doc: DocId) -> ShardId {
    let shard = (doc >> LOCAL_BITS) & u64::from(MAX_SHARD_ID);
    ShardId(u16::try_from(shard).unwrap_or(MAX_SHARD_ID))
}

/// Shard-local id contained in a globally packed document ID.
pub const fn local_of(doc: DocId) -> LocalDocId {
    doc & MAX_LOCAL_DOC_ID
}

//each exact fields index starts with this token, some random bullshit + whitespace (because no user
//inserted tokens can have a whitespace + length (token count) for exact-field
const EXACT_MARKER_PREFIX: &str = "\u{0}\u{1}exact\u{1} ";
///final marker token at the start of every exact value/element + carries its token count.
#[must_use]
pub fn exact_len_marker(tokens: usize) -> String {
    format!("{EXACT_MARKER_PREFIX}len:{tokens}")
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
        Self(raw)
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

pub const fn position_start(position: Position) -> u32 {
    position >> SPAN_BITS
}

pub const fn position_span(position: Position) -> u32 {
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
    pub const fn add(&mut self, other: &Self) {
        self.doc_count += other.doc_count;
        self.total_doc_len += other.total_doc_len;
    }
}
