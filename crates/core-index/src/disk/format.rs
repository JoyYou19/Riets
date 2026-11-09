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
//! (doc_id u64, parent u64) × len
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

//                          ahahahahhahah
pub const MAGIC: [u8; 8] = *b"BANANA_4";
pub const VERSION: u32 = 12;

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
