//! Segment file layout (little-endian). Postings are LEB128 varints (see `codec.rs`);
//! every other section is fixed-width little-endian (`write_u32` / `write_u64`).
//!
//! A segment is written once and read via mmap. Everything after the header is a set
//! of blobs whose offsets/lengths are recorded in the footer.
//!
//! ```text
//! +------------------+
//! | header           |  magic + version
//! +------------------+
//! | postings         |  one varint blob per term (see "posting blob" below)
//! +------------------+
//! | doc_lengths      |  token counts per (doc, field) for BM25 length norm
//! +------------------+
//! | dictionary       |  FST: term -> ord, plus per-ord posting metadata
//! +------------------+
//! | numeric_fields   |  BKD points + doc values for numeric / date fields
//! +------------------+
//! | array_row_index  |  array-row id -> owning doc / parent row
//! +------------------+
//! | bool_fields      |  per field, two RoaringTreemaps (true / false ids)
//! +------------------+
//! | footer           |  fixed 100 bytes of offsets + counts
//! +------------------+
//! ```
//!
//! ## header  (12 bytes, raw LE)
//!
//! | field   | size | meaning                                  |
//! |---------|------|------------------------------------------|
//! | magic   | 8    | `BANANA_4` — sanity check               |
//! | version | 4    | format version                          |
//!
//!
//! A posting list is a run of documents; each document carries its occurrences
//! (word positions) and, when present, the byte spans used for highlighting.
//!
//! ```text
//! doc_delta        var u64   doc_id - previous doc_id (first is absolute)
//! weight           var u16   max term weight in this doc (WAND upper bound)
//! position_count   var u32   occurrences of this term in this doc
//! offsets_present  u8        raw byte (not varint): 1 = offsets follow, 0 = none
//! position_delta   var u32 × position_count   word position - previous position
//!
//! -- if offsets_present (for non-char-filtered fields) --
//! start_delta      var u32 × position_count   start_byte - previous end_byte
//! length           var u32 × position_count   end_byte - start_byte
//! ```
//!
//! ## doc_lengths  
//!
//! ```text
//! count u32
//! (doc_id u64, xpath u32, len u32) × count
//! ```
//!
//! `len` is the number of tokens this field added to the document; used for the
//! BM25 average-length normalization.
//!
//! ## dictionary  (raw LE)
//!
//! ```text
//! field_count u32
//! per field:
//!     xpath u32
//!     term_count u32
//!     fst_len u64
//!     fst_bytes[fst_len]                       // maps term bytes -> ord
//!     (postings_offset u64, postings_len u32,
//!      doc_freq u32, max_weight u16) × term_count
//! ```
//!
//! The FST assigns each term an ord; the fixed-width meta array, indexed by that
//! ord, points into the `postings` section for that term.
//!
//! ## numeric_fields  (raw LE)
//!
//! ```text
//! xpath_count u32
//! per xpath:
//!     xpath u32
//!     kind u8                       // Integer / Float / Date / ...
//!     bkd_point_count u32
//!     (packed_value u64, doc_id u64) × bkd_point_count      // sorted by value
//!     doc_value_entry_count u32
//!     (doc_id u64, packed_value u64) × doc_value_entry_count // sorted by doc_id
//! ```
//!
//! Used for range filters and sorting. A `Date` field stores its UTC epoch
//! milliseconds here, so sort/order is a plain numeric comparison.
//!
//! ## array_row_index  (raw LE)
//!
//! ```text
//! base u64
//! len u32
//! (doc_id u64, parent u64, element_index u32) × len
//! ```
//!
//! Maps each array row id to the document (or parent row) that owns it, so a
//! match inside a nested array element resolves back to its document.
//!
//! ## bool_fields  (raw LE)
//!
//! ```text
//! field_count u32
//! per field:
//!     xpath u32
//!     true_len u64
//!     true_bytes[true_len]    // RoaringTreemap of doc/row ids that are `true`
//!     false_len u64
//!     false_bytes[false_len]  // RoaringTreemap of doc/row ids that are `false`
//! ```
//!
//! A document where the bool field is absent is in neither bitmap (missing != false).
//!
//! ## footer  (100 bytes, raw LE, at the end of the file)
//!
//! ```text
//! +0   doc_lengths_offset      u64
//! +8   doc_lengths_len         u64
//! +16  dictionary_offset       u64
//! +24  dictionary_len          u64
//! +32  numeric_fields_offset   u64
//! +40  numeric_fields_len      u64
//! +48  term_count              u32
//! +52  min_doc_id              u64
//! +60  max_doc_id              u64
//! +68  array_row_index_offset  u64
//! +76  array_row_index_len     u64
//! +84  bool_fields_offset      u64
//! +92  bool_fields_len         u64
//! ```

use std::io;

use regex::bytes;

//                          ahahahahhahah
pub const MAGIC: [u8; 8] = *b"BANANA_4";
pub const VERSION: u32 = 14;

pub const HEADER_LEN: usize = 8 + 4;
pub const BLOCK_LEN:usize =128;
//100B
pub const FOOTER_LEN: usize = 8 + 8 + 8 + 8 + 8 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 8;
//skips entries for things in blocks
pub const SKIP_ENTRY_LEN: usize = 8 + 8 + 4 + 4 + 4 + 4 + 4 + 2 + 1;

fn invalid(message:&'static str) -> io::Error{
        io::Error::new(io::ErrorKind::InvalidData, message)
    }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
//posting block entry for skipping the block
pub struct SkipEntry{
    pub first_doc_id:u64,
    pub last_doc_id :u64,
    //byte offset of the block realtive to start of term blocks
    pub block_start_offset:u32,
    //byte offset of first document in terms positions
    pub positions_start_offset:u32,
    //byte offset of blocks first document in terms offset
    pub offsets_start_offset:u32,
    //max term frequency
    pub max_tf:u32,
    //shortest doc
    pub min_doc_len:u32,
    //max weight in block
    pub max_weight:u16,
    //docs in block up to 128
    pub len:u8
}
impl SkipEntry{
    pub fn write_to(&self, out:&mut Vec<u8>){
        out.extend_from_slice(&self.first_doc_id.to_le_bytes());
        out.extend_from_slice(&self.last_doc_id.to_le_bytes());
        out.extend_from_slice(&self.block_start_offset.to_le_bytes());
        out.extend_from_slice(&self.positions_start_offset.to_le_bytes());
        out.extend_from_slice(&self.offsets_start_offset.to_le_bytes());
        out.extend_from_slice(&self.max_tf.to_le_bytes());
        out.extend_from_slice(&self.min_doc_len.to_le_bytes());
        out.extend_from_slice(&self.max_weight.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
    }
    pub fn read_at(table:&[u8], index:usize)-> io::Result<Self>{
        let start=index.checked_mul(SKIP_ENTRY_LEN).ok_or_else(|| invalid("skip entry index overflow"))?;
        let bytes=table.get(start..start+SKIP_ENTRY_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "skip entry past end"))?;
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));

        let entry = Self {
            first_doc_id: u64_at(0),
            last_doc_id: u64_at(8),
            block_start_offset: u32_at(16),
            positions_start_offset: u32_at(20),
            offsets_start_offset: u32_at(24),
            max_tf: u32_at(28),
            min_doc_len: u32_at(32),
            max_weight: u16::from_le_bytes([bytes[36], bytes[37]]),
            len: bytes[38],
        };
        if entry.len ==0|| usize::from(entry.len) >BLOCK_LEN|| entry.last_doc_id<entry.first_doc_id{
            return Err(invalid("corrupt skip entry"));
        }
        Ok(entry)
    }
    pub fn last_doc_at(table:&[u8], index:usize) -> io::Result<u64>{
        let start =index 
        .checked_mul(SKIP_ENTRY_LEN)
        .and_then(|at| at.checked_add(8))
        .ok_or_else(|| invalid("skip entry index overflow"))?;
        let bytes=table.get(start..start+8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "skip entry past end "))?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
    }
    
}
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
