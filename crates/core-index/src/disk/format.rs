use crate::types::XPathId;

pub const MAGIC: [u8; 8] = *b"CLIDX001";
pub const VERSION: u32 = 4;

pub const HEADER_LEN: usize = 8 + 4;
pub const FOOTER_LEN: usize = 8 + 8 + 8 + 8 + 4;

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
    pub term_count: u32,
}
