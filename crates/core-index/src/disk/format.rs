//INFO: paldies chatin par so komentaru

//! Segment file layout (little-endian):
//! ```text
//! +------------------------+
//! | header                 |  magic[8] + version[4]                (12 bytes)
//! +------------------------+
//! | postings               |  raw posting bytes, one blob per term
//! +------------------------+
//! | doc_lengths            |  u32 count, then (u64 doc_id, u32 xpath, u32 len)*
//! +------------------------+
//! | dictionary             |  u32 field_count, then per field:
//! |                        |    u32 xpath, u32 term_count, u64 fst_len,
//! |                        |    fst_bytes[fst_len],
//! |                        |    (u64 postings_offset, u32 postings_len,
//! |                        |     u32 doc_freq, u16 max_weight) * term_count
//! |                        |  the FST maps term bytes -> ord, and the
//! |                        |  fixed width meta array is indexed by that ord
//! +------------------------+
//! | numeric_fields         |  u32 xpath_count, then per xpath:
//! |                        |    u32 xpath, u8 kind,
//! |                        |    u32 bkd_point_count,
//! |                        |    (u64 packed_value, u64 doc_id) * bkd_point_count,
//! |                        |    u32 doc_value_entry_count,
//! |                        |    (u64 doc_id, u64 packed_value) * doc_value_entry_count
//! +------------------------+
//! | footer                 |  doc_lengths_offset/len, dictionary_offset/len,
//! |                        |  numeric_fields_offset/len, term_count  (52 bytes)
//! +------------------------+
//! //! |    (now with the FST)  |
//! |                        |    u32 xpath, u32 term_count, u64 fst_len,
//! |                        |    fst_bytes[fst_len],
//! |                        |    (u64 postings_offset, u32 postings_len,
//! |                        |     u32 doc_freq, u16 max_weight) * term_count
//! ```
//!

//                          ahahahahhahah
pub const MAGIC: [u8; 8] = *b"BANANA_I";
pub const VERSION: u32 = 9;

pub const HEADER_LEN: usize = 8 + 4;
pub const FOOTER_LEN: usize = 8 + 8 + 8 + 8 + 8 + 8 + 4;

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
    pub term_count: u32,
}
