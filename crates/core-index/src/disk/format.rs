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
//! | dictionary             |  u32 count, then TermEntry*
//! +------------------------+
//! | columns                |  u32 xpath_count, then per xpath:false
//! |                        |    u32 xpath, u32 entry_count,
//! |                        |    (u8 kind, u64 value_bits, u64 doc_id)*
//! +------------------------+
//! | footer                 |  doc_lengths_offset/len, dictionary_offset/len,
//! |                        |  columns_offset/len, term_count           (52 bytes)
//! +------------------------+
//! ```

use crate::types::XPathId;

pub const MAGIC: [u8; 8] = *b"CLIDX001";
pub const VERSION: u32 = 5;

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
pub struct TermEntry {
    pub term: String,
    pub xpath: XPathId,
    pub postings_offset: u64,
    pub postings_len: u32,
    pub doc_freq: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFooter {
    pub doc_lengths_offset: u64,
    pub doc_lengths_len: u64,
    pub dictionary_offset: u64,
    pub dictionary_len: u64,
    pub columns_offset: u64,
    pub columns_len: u64,
    pub term_count: u32,
}
