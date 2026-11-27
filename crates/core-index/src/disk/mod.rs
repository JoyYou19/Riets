pub mod codec;
pub mod format;
pub mod reader;
pub mod writer;
pub mod block_cursor;
use std::sync::Arc;

use memmap2::Mmap;

use crate::disk::SegmentBytes::Owned;


/// Bytes built in memory, or a range of a segment's mmap.
pub enum SegmentBytes {
    Owned(Vec<u8>),
    // the range is checked against the mapping when it is made
    Mapped { mmap: Arc<Mmap>, start: usize, end: usize },
}

impl Default for SegmentBytes {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}



impl AsRef<[u8]> for SegmentBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped { mmap, start, end } => mmap.get(*start..*end).unwrap_or_default(),
        }
    }
}