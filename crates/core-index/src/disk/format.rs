//INFO: paldies chatin par so komentaru

//! Segment file layout (little-endian):
//! +------------------+
//! | header           |  magic[8] + version[4]                       (12 bytes)
//! +------------------+
//! | postings         |  raw posting bytes, one varint blob per term
//! +------------------+
//! | doc_lengths      |  u32 count, then (u64 doc_id, u32 xpath, u32 len) * count
//! +------------------+
//! | dictionary       |  u32 field_count, then per field:
//! |                  |    u32 xpath, u32 term_count, u64 fst_len,
//! |                  |    fst_bytes[fst_len],
//! |                  |    (u64 postings_offset, u32 postings_len,
//! |                  |     u32 doc_freq, u16 max_weight) * term_count
//! |                  |  the FST maps term bytes -> ord; the fixed-width meta
//! |                  |  array is indexed by that ord
//! +------------------+
//! | numeric_fields   |  u32 xpath_count, then per xpath:
//! |                  |    u32 xpath, u8 kind,
//! |                  |    u32 bkd_point_count,
//! |                  |    (u64 packed_value, u64 doc_id) * bkd_point_count,
//! |                  |    u32 doc_value_entry_count,
//! |                  |    (u64 doc_id, u64 packed_value) * doc_value_entry_count
//! +------------------+
//! | array_row_index  |  u64 base, u32 len,
//! |                  |    (u64 doc_id, u64 parent) * len
//! +------------------+
//! | bool_fields      |  u32 field_count, then per field:
//! |                  |    u32 xpath, u64 bitmap_len, bitmap_bytes[bitmap_len]
//! |                  |  (each bitmap is a RoaringTreemap of the doc/row ids
//! |                  |   whose value is `true`)
//! +------------------+
//! | footer           |  u64 doc_lengths_offset, u64 doc_lengths_len,
//! |                  |  u64 dictionary_offset, u64 dictionary_len,
//! |                  |  u64 numeric_fields_offset, u64 numeric_fields_len,
//! |                  |  u32 term_count,
//! |                  |  u64 min_doc_id, u64 max_doc_id,
//! |                  |  u64 array_row_index_offset, u64 array_row_index_len,
//! |                  |  u64 bool_fields_offset, u64 bool_fields_len       (100 bytes)
//! +------------------+
//! ```

//                          ahahahahhahah
pub const MAGIC: [u8; 8] = *b"BANANA_3";
pub const VERSION: u32 = 11;

pub const HEADER_LEN: usize = 8 + 4;

//100B
pub const FOOTER_LEN: usize = 8 + 8 + 8 + 8 + 8 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub magic: [u8; 8],
    pub version: u32,
}

impl SegmentHeader {
    pub fn current() -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFooter {
    pub doc_lengths_offset: u64,
    pub doc_lengths_len: u64,
    pub dictionary_offset: u64,
    pub dictionary_len: u64,
    pub numeric_fields_offset: u64,
    pub numeric_fields_len: u64,
    pub array_row_index_offset: u64,
    pub array_row_index_len: u64,
    pub term_count: u32,
    pub min_doc_id: u64,
    pub max_doc_id: u64,
    pub bool_fields_offset: u64,
    pub bool_fields_len: u64,
}
