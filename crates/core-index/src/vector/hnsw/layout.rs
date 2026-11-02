//! Byte layout of a `.hnsw` graph file: the one definition shared by the builder (writer)
//! and `HnswGraph` (reader).
//!
//! Layout (little-endian), every section starting on a 64-byte boundary:
//! +----------------------+
//! | header               |  magic[8] "CLHNSW02", u32 version, u32 reserved,
//! |                      |  u64 vector_file_id, u32 node_count, u32 dims,
//! |                      |  u16 max_links_upper, u16 max_links_level0, u32 entry_node,
//! |                      |  u8 top_level, u8[3] reserved, u32 upper_slot_count
//! | node levels          |  u8 * node_count            highest level of each node
//! | upper slot starts    |  u32 * node_count           first upper slot of each node
//! | level 0 lists        |  slot * node_count          slot = u32 len + u32[max_links_level0]
//! | upper level lists    |  slot * upper_slot_count    slot = u32 len + u32[max_links_upper]
//! +----------------------+
//!
//! A node at level L >= 1 owns L consecutive upper slots: its list on level l is at
//! `upper_slot_start + (l - 1)`. Nodes at level 0 own none; their start is `NO_UPPER_SLOTS`.
//! node: one row of the vector file. Node n is row n.
//!level: a layer of the graph. Level 0 contains every node; higher levels contain fewer.
//!neighbour list: the nodes one node links to on one level.
//!list slot: the fixed-size space that holds one neighbour list: a u32 length followed by room for the maximum number of links. Fixed size means a list’s position can be computed instead of searched for.

use std::io::{self, Write};
use std::path::Path;

use crate::vector::file::{SECTION_ALIGNMENT, write_zero_padding};

pub(crate) const HNSW_MAGIC: [u8; 8] = *b"CLHNSW02";
pub(crate) const HNSW_VERSION: u32 = 2;

/// Largest allowed `max_links_upper` (HNSW's `M`); level 0 allows twice as many.
pub(crate) const MAX_LINKS_LIMIT: usize = 128;
/// Highest level a node can get; keeps levels in a `u8` whatever the random draw says.
pub(crate) const MAX_LEVEL: u8 = 16;
/// Upper slot start of a node that only lives on level 0.
pub(crate) const NO_UPPER_SLOTS: u32 = u32::MAX;

/// magic + version + reserved + vector_file_id + node_count + dims + max_links_upper +
/// max_links_level0 + entry_node + top_level + reserved + upper_slot_count
const HEADER_FIXED_LENGTH: usize = 8 + 4 + 4 + 8 + 4 + 4 + 2 + 2 + 4 + 1 + 3 + 4;

/// Everything stored in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GraphHeader {
    /// `file_id` of the vector file this graph was built from.
    pub(crate) vector_file_id: u64,
    pub(crate) node_count: u32,
    pub(crate) dims: u32,
    /// Most links a node keeps on levels >= 1 (HNSW's `M`).
    pub(crate) max_links_upper: u16,
    /// Most links a node keeps on level 0 (HNSW's `M0`, = 2 * M).
    pub(crate) max_links_level0: u16,
    /// Node where every search starts; it is on `top_level`.
    pub(crate) entry_node: u32,
    pub(crate) top_level: u8,
    /// Total upper slots: the sum of all node levels.
    pub(crate) upper_slot_count: u32,
}

impl GraphHeader {
    pub(crate) fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&HNSW_MAGIC)?;
        out.write_all(&HNSW_VERSION.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&self.vector_file_id.to_le_bytes())?;
        out.write_all(&self.node_count.to_le_bytes())?;
        out.write_all(&self.dims.to_le_bytes())?;
        out.write_all(&self.max_links_upper.to_le_bytes())?;
        out.write_all(&self.max_links_level0.to_le_bytes())?;
        out.write_all(&self.entry_node.to_le_bytes())?;
        out.write_all(&[self.top_level, 0, 0, 0])?;
        out.write_all(&self.upper_slot_count.to_le_bytes())?;
        write_zero_padding(out, HEADER_FIXED_LENGTH, header_length())
    }

    /// Parses and sanity-checks the header. Errors are `InvalidData`, naming `path`.
    pub(crate) fn read_from(bytes: &[u8], path: &Path) -> io::Result<Self> {
        if bytes.len() < header_length() {
            return Err(corrupt(path, "shorter than its header"));
        }
        if bytes[0..8] != HNSW_MAGIC {
            return Err(corrupt(path, "bad magic"));
        }
        let version = read_u32(bytes, 8);
        if version != HNSW_VERSION {
            return Err(corrupt(path, format!("unsupported version {version}")));
        }
        let header = Self {
            vector_file_id: read_u64(bytes, 16),
            node_count: read_u32(bytes, 24),
            dims: read_u32(bytes, 28),
            max_links_upper: read_u16(bytes, 32),
            max_links_level0: read_u16(bytes, 34),
            entry_node: read_u32(bytes, 36),
            top_level: bytes[40],
            upper_slot_count: read_u32(bytes, 44),
        };
        header.validate().map_err(|msg| corrupt(path, msg))?;
        Ok(header)
    }

    /// Checks the header is internally consistent; the caller checks it against the
    /// vector file (file id, node count, dims).
    pub(crate) fn validate(&self) -> Result<(), String> {
        let upper = usize::from(self.max_links_upper);
        if !(2..=MAX_LINKS_LIMIT).contains(&upper) {
            return Err(format!("max_links_upper {upper} outside 2..={MAX_LINKS_LIMIT}"));
        }
        if usize::from(self.max_links_level0) != upper * 2 {
            return Err(format!(
                "max_links_level0 {} is not twice max_links_upper {upper}",
                self.max_links_level0
            ));
        }
        if self.node_count == 0 {
            return Err("graph has no nodes".to_string());
        }
        if self.entry_node >= self.node_count {
            return Err(format!("entry node {} outside {} nodes", self.entry_node, self.node_count));
        }
        if self.top_level > MAX_LEVEL {
            return Err(format!("top level {} above {MAX_LEVEL}", self.top_level));
        }
        Ok(())
    }
}

/// Header size including padding: the offset of the first section.
pub(crate) const fn header_length() -> usize {
    HEADER_FIXED_LENGTH.next_multiple_of(SECTION_ALIGNMENT)
}

/// Byte offset of every section, and the size of each list slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GraphLayout {
    pub(crate) node_levels_offset: usize,
    pub(crate) upper_slot_starts_offset: usize,
    pub(crate) level0_lists_offset: usize,
    pub(crate) upper_lists_offset: usize,
    pub(crate) total_length: usize,
    /// Bytes per level-0 slot: (1 + max_links_level0) * 4.
    pub(crate) level0_slot_length: usize,
    /// Bytes per upper slot: (1 + max_links_upper) * 4.
    pub(crate) upper_slot_length: usize,
}

impl GraphLayout {
    /// Offsets for `header`; `None` if the file would not fit in `usize`.
    pub(crate) fn new(header: &GraphHeader) -> Option<Self> {
        let nodes = header.node_count as usize;
        let level0_slot_length = (1 + usize::from(header.max_links_level0)) * 4;
        let upper_slot_length = (1 + usize::from(header.max_links_upper)) * 4;

        let node_levels_offset = header_length();
        let upper_slot_starts_offset =
            next_section(node_levels_offset.checked_add(nodes)?)?;
        let level0_lists_offset =
            next_section(upper_slot_starts_offset.checked_add(nodes.checked_mul(4)?)?)?;
        let upper_lists_offset = next_section(
            level0_lists_offset.checked_add(nodes.checked_mul(level0_slot_length)?)?,
        )?;
        let total_length = upper_lists_offset
            .checked_add((header.upper_slot_count as usize).checked_mul(upper_slot_length)?)?;

        Some(Self {
            node_levels_offset,
            upper_slot_starts_offset,
            level0_lists_offset,
            upper_lists_offset,
            total_length,
            level0_slot_length,
            upper_slot_length,
        })
    }

    /// Byte offset of the slot holding `node`'s neighbour list on `level`.
    /// For `level >= 1`, `upper_slot_start` is the node's entry in the upper slot starts
    /// section, and the node must be on that level.
    pub(crate) fn slot_offset(&self, node: usize, level: usize, upper_slot_start: u32) -> usize {
        if level == 0 {
            self.level0_lists_offset + node * self.level0_slot_length
        } else {
            debug_assert_ne!(upper_slot_start, NO_UPPER_SLOTS, "node has no upper levels");
            let slot = upper_slot_start as usize + (level - 1);
            self.upper_lists_offset + slot * self.upper_slot_length
        }
    }
}

/// Rounds up to the next section boundary.
fn next_section(offset: usize) -> Option<usize> {
    offset.checked_next_multiple_of(SECTION_ALIGNMENT)
}

fn corrupt(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("hnsw file {}: {msg}", path.display()),
    )
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub(crate) fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut buffer = [0u8; 8];
    buffer.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> GraphHeader {
        GraphHeader {
            vector_file_id: 0xDEAD_BEEF_1234_5678,
            node_count: 1_000,
            dims: 384,
            max_links_upper: 16,
            max_links_level0: 32,
            entry_node: 7,
            top_level: 3,
            upper_slot_count: 70,
        }
    }

    #[test]
    fn header_round_trip() {
        let header = sample_header();
        let mut bytes = Vec::new();
        header.write_to(&mut bytes).expect("write");
        assert_eq!(bytes.len(), header_length());
        let read = GraphHeader::read_from(&bytes, Path::new("x.hnsw")).expect("read");
        assert_eq!(read, header);
    }

    #[test]
    fn bad_headers_are_invalid_data() {
        let mut bytes = Vec::new();
        sample_header().write_to(&mut bytes).expect("write");
        let path = Path::new("x.hnsw");

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        let mut bad_version = bytes.clone();
        bad_version[8..12].copy_from_slice(&1u32.to_le_bytes());
        let mut bad_links = bytes.clone();
        bad_links[34..36].copy_from_slice(&31u16.to_le_bytes());
        let mut bad_entry = bytes.clone();
        bad_entry[36..40].copy_from_slice(&5_000u32.to_le_bytes());

        for (name, case) in [
            ("magic", bad_magic),
            ("version", bad_version),
            ("links", bad_links),
            ("entry", bad_entry),
        ] {
            let err = GraphHeader::read_from(&case, path).expect_err(name);
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{name}");
        }
        assert!(GraphHeader::read_from(&bytes[..10], path).is_err(), "truncated");
    }

    #[test]
    fn sections_are_aligned_and_do_not_overlap() {
        let header = sample_header();
        let layout = GraphLayout::new(&header).expect("layout");
        let nodes = header.node_count as usize;
        let offsets = [
            layout.node_levels_offset,
            layout.upper_slot_starts_offset,
            layout.level0_lists_offset,
            layout.upper_lists_offset,
        ];
        assert!(offsets.iter().all(|o| o % SECTION_ALIGNMENT == 0));
        assert!(layout.upper_slot_starts_offset >= layout.node_levels_offset + nodes);
        assert!(layout.level0_lists_offset >= layout.upper_slot_starts_offset + nodes * 4);
        assert!(layout.upper_lists_offset >= layout.level0_lists_offset + nodes * layout.level0_slot_length);
        assert_eq!(layout.total_length, layout.upper_lists_offset + 70 * layout.upper_slot_length);
    }

    #[test]
    fn slot_offsets_follow_the_layout() {
        let layout = GraphLayout::new(&sample_header()).expect("layout");
        assert_eq!(layout.slot_offset(0, 0, NO_UPPER_SLOTS), layout.level0_lists_offset);
        assert_eq!(
            layout.slot_offset(2, 0, NO_UPPER_SLOTS),
            layout.level0_lists_offset + 2 * layout.level0_slot_length
        );
        assert_eq!(
            layout.slot_offset(9, 2, 10),
            layout.upper_lists_offset + 11 * layout.upper_slot_length
        );
    }
}