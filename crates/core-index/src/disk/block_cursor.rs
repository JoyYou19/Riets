use std::io;

use bitpacking::{ BitPacker, BitPacker4x };

use crate::{
    disk::{
        codec::{ DecodedBlock, decode_block, read_var_u32, skip_varints },
        format::{ SKIP_ENTRY_LEN, SkipEntry },
    },
    term_dict::TermMeta,
    types::{ DocId, position_start },
};

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// `(term frequency of slot, sum of the term frequencies before it)`; `None` if `slot` is out of range.
fn freq_and_before(term_freq: &[u32], slot: usize) -> Option<(u32, usize)> {
    let freq = *term_freq.get(slot)?;
    let before = term_freq.get(..slot)?.iter().fold(0usize, |acc, &t| {
        acc.saturating_add(usize::try_from(t).unwrap_or(usize::MAX))
    });
    Some((freq, before))
}

fn decode_positions(
    positions: &[u8],
    start: usize,
    before: usize,
    freq: u32,
    out: &mut Vec<u32>
) -> io::Result<()> {
    let mut at = start;
    skip_varints(positions, &mut at, before)?;
    let mut position = 0u32;
    for _ in 0..freq {
        let delta = read_var_u32(positions, &mut at)?;
        position = position.checked_add(delta).ok_or_else(|| invalid("position overflow"))?;
        out.push(position);
    }
    Ok(())
}

/// One term's postings region, split into its four parts.
pub(crate) struct TermRegion<'a> {
    pub skip_table: &'a [u8],
    pub blocks: &'a [u8],
    pub positions: &'a [u8],
    pub offsets: &'a [u8],
    pub block_count: usize,
}

impl<'a> TermRegion<'a> {
    /// Splits `[skip table][blocks][positions][offsets]` using the lengths in `meta`.
    pub(crate) fn split(bytes: &'a [u8], meta: TermMeta) -> io::Result<Self> {
        let truncated = || io::Error::new(io::ErrorKind::UnexpectedEof, "term region truncated");

        let block_count = usize::try_from(meta.block_count).map_err(invalid)?;
        let skip_len = block_count
            .checked_mul(SKIP_ENTRY_LEN)
            .ok_or_else(|| invalid("skip table size overflow"))?;
        let blocks_len = usize::try_from(meta.block_len).map_err(invalid)?;
        let positions_len = usize::try_from(meta.positions_len).map_err(invalid)?;

        let (skip_table, rest) = bytes.split_at_checked(skip_len).ok_or_else(truncated)?;
        let (blocks, rest) = rest.split_at_checked(blocks_len).ok_or_else(truncated)?;
        let (positions, offsets) = rest.split_at_checked(positions_len).ok_or_else(truncated)?;

        Ok(Self { skip_table, blocks, positions, offsets, block_count })
    }
}

/// Forward-only cursor over one term in one disk segment. Decodes only the blocks it lands in;
/// blocks it jumps over are never read.
pub struct BlockCursor<'a> {
    packer: BitPacker4x,
    region: TermRegion<'a>,
    block_index: usize,
    entry: SkipEntry,
    block: Option<Box<DecodedBlock>>,
    loaded: bool,
    slot: usize,
    exhausted: bool,
    /// Decoded positions for the current `slot`, lazily filled.
    position_cache: Vec<u32>,
    /// Read head inside `position_cache`.
    position_index: usize,
}

impl<'a> BlockCursor<'a> {
    pub(crate) fn new(bytes: &'a [u8], meta: TermMeta) -> io::Result<Self> {
        let region = TermRegion::split(bytes, meta)?;
        let exhausted = region.block_count == 0;
        let entry = if exhausted {
            SkipEntry::default()
        } else {
            SkipEntry::read_at(region.skip_table, 0)?
        };
        Ok(Self {
            packer: BitPacker4x::new(),
            exhausted,
            region,
            block_index: 0,
            entry,
            block: None,
            loaded: false,
            slot: 0,
            position_cache: Vec::new(),
            position_index: 0,
        })
    }

    fn doc_at_slot(&self) -> Option<DocId> {
        self.block.as_ref()?.docs.get(self.slot).copied()
    }

    fn freq_at_slot(&self) -> Option<u32> {
        self.block.as_ref()?.term_freq.get(self.slot).copied()
    }

    fn weight_at_slot(&self) -> Option<u16> {
        self.block.as_ref()?.weights.get(self.slot).copied()
    }

    /// Decodes block `index` and puts the cursor on its first document. Exhausts the cursor when
    /// there is no such block or it fails to decode.
    fn load_block(&mut self, index: usize) -> bool {
        if index >= self.region.block_count || self.try_load_block(index).is_err() {
            self.exhausted = true;
            return false;
        }
        true
    }

    fn try_load_block(&mut self, index: usize) -> io::Result<()> {
        // Slot is about to change: discard any per-slot position state.
        self.position_cache.clear();
        self.position_index = 0;

        let entry = SkipEntry::read_at(self.region.skip_table, index)?;
        let start = usize::try_from(entry.block_start_offset).map_err(invalid)?;
        let bytes = self.region.blocks
            .get(start..)
            .ok_or_else(|| invalid("block offset past blocks area"))?;
        let block = self.block.get_or_insert_with(Box::default);
        decode_block(self.packer, bytes, &entry, block)?;
        self.entry = entry;
        self.block_index = index;
        self.slot = 0;
        self.loaded = true;
        Ok(())
    }

    #[inline]
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    #[inline]
    #[must_use]
    pub fn doc_id(&self) -> Option<DocId> {
        if self.exhausted {
            None
        } else if !self.loaded {
            Some(self.entry.first_doc_id) // the cursor is on block 0's first document
        } else {
            self.doc_at_slot()
        }
    }

    #[inline]
    pub fn term_freq(&mut self) -> u32 {
        self.ensure_loaded();
        if self.exhausted {
            return 0;
        }
        self.freq_at_slot().unwrap_or(0)
    }

    /// Weight of the current document; 0 when exhausted.
    #[inline]
    pub fn weight(&mut self) -> u16 {
        self.ensure_loaded();
        if self.exhausted {
            return 0;
        }
        self.weight_at_slot().unwrap_or(0)
    }

    /// The skip entry of the block the cursor is in (block bounds for block-max WAND).
    #[inline]
    #[must_use]
    pub const fn block_entry(&self) -> &SkipEntry {
        &self.entry
    }

    #[inline]
    fn ensure_loaded(&mut self) {
        if !self.loaded && !self.exhausted {
            self.load_block(0);
        }
    }

    pub fn next(&mut self) {
        self.ensure_loaded();
        if self.exhausted {
            return;
        }
        self.slot += 1;
        self.position_cache.clear();
        self.position_index = 0;
        let block_len = self.block.as_ref().map_or(0, |b| b.len);
        if self.slot >= block_len {
            self.load_block(self.block_index + 1);
        }
    }

    /// Moves to the first document with id >= `target`. Never moves backwards.
    pub fn advance_to(&mut self, target: DocId) {
        if self.exhausted {
            return;
        }
        // a fresh cursor sits on block 0's first doc without having decoded it
        if !self.loaded && target <= self.entry.first_doc_id {
            return;
        }
        let current_last = self.entry.last_doc_id;
        let current_doc = if self.loaded {
            let Some(doc) = self.doc_at_slot() else {
                self.exhausted = true;
                return;
            };
            doc
        } else {
            self.entry.first_doc_id
        };
        if self.loaded && current_doc >= target {
            return;
        }

        if current_last < target {
            // the target is in a later block: binary-search the skip table, decode only that block
            let mut low = if self.loaded { self.block_index + 1 } else { 1 };
            let mut high = self.region.block_count;
            while low < high {
                let mid = low + (high - low) / 2;
                match SkipEntry::last_doc_at(self.region.skip_table, mid) {
                    Ok(last) if last < target => {
                        low = mid + 1;
                    }
                    Ok(_) => {
                        high = mid;
                    }
                    Err(_) => {
                        self.exhausted = true;
                        return;
                    }
                }
            }
            if !self.load_block(low) {
                return;
            }
        } else if !self.loaded {
            // the target is inside block 0
            if !self.load_block(0) {
                return;
            }
        }

        let Some(block) = self.block.as_ref() else {
            self.exhausted = true;
            return;
        };
        let Some(remaining) = block.docs.get(self.slot..block.len) else {
            self.exhausted = true;
            return;
        };
        self.slot += remaining.partition_point(|&doc| doc < target);
        let past_end = self.slot >= block.len;
        self.position_cache.clear();
        self.position_index = 0;

        if past_end {
            self.load_block(self.block_index + 1);
        }
    }

    /// Positions of the current document, appended to `out` (which is cleared first).
    pub fn positions_into(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        self.ensure_loaded();
        self.decode_current_positions(out)
    }

    pub fn advance_to_position(&mut self, target: u32) -> io::Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        self.ensure_positions_decoded()?;
        while self.position_cache
            .get(self.position_index)
            .is_some_and(|&p| position_start(p) < target)
        {
            self.position_index += 1;
        }
        Ok(self.position_index < self.position_cache.len())
    }

    /// The position `advance_to_position` is currently on, if any.
    #[must_use]
    pub fn current_position(&self) -> Option<u32> {
        self.position_cache.get(self.position_index).copied()
    }

    fn ensure_positions_decoded(&mut self) -> io::Result<()> {
        if !self.position_cache.is_empty() || self.term_freq() == 0 || self.exhausted {
            return Ok(()); // term_freq() also loads the block
        }
        let Some((freq, before)) = self.block
            .as_ref()
            .and_then(|b| freq_and_before(&b.term_freq, self.slot)) else {
            return Ok(());
        };
        let start = usize::try_from(self.entry.positions_start_offset).map_err(invalid)?;
        decode_positions(self.region.positions, start, before, freq, &mut self.position_cache)
    }

    fn decode_current_positions(&self, out: &mut Vec<u32>) -> io::Result<()> {
        out.clear();
        let Some(block) = self.block.as_ref().filter(|_| !self.exhausted && self.loaded) else {
            return Ok(());
        };
        let Some((freq, before)) = freq_and_before(&block.term_freq, self.slot) else {
            return Ok(());
        };
        let start = usize::try_from(self.entry.positions_start_offset).map_err(invalid)?;
        decode_positions(self.region.positions, start, before, freq, out)
    }

    /// Highlight offsets of the current document, appended to `out` (cleared first). Empty when
    /// the document was indexed without offsets.
    pub fn offsets_into(&mut self, out: &mut Vec<(u32, u32)>) -> io::Result<()> {
        out.clear();
        self.ensure_loaded();
        let Some(block) = self.block.as_ref().filter(|_| !self.exhausted) else {
            return Ok(());
        };
        let offsets = self.region.offsets;
        let mut at = usize::try_from(self.entry.offsets_start_offset).map_err(invalid)?;

        let present_at = |index: usize| -> io::Result<bool> {
            offsets
                .get(index)
                .map(|&flag| flag != 0)
                .ok_or_else(||
                    io::Error::new(io::ErrorKind::UnexpectedEof, "missing offsets flag")
                )
        };

        let (Some(earlier), Some(&current_freq)) = (
            block.term_freq.get(..self.slot),
            block.term_freq.get(self.slot),
        ) else {
            return Err(invalid("slot outside block"));
        };

        for &tf in earlier {
            let present = present_at(at)?;
            at += 1;
            if present {
                let varints = usize::try_from(tf).map_err(invalid)?.saturating_mul(2);
                skip_varints(offsets, &mut at, varints)?;
            }
        }

        if present_at(at)? {
            at += 1;
            for _ in 0..current_freq {
                let start = read_var_u32(offsets, &mut at)?;
                let len = read_var_u32(offsets, &mut at)?;
                let end = start.checked_add(len).ok_or_else(|| invalid("offset overflow"))?;
                out.push((start, end));
            }
        }
        Ok(())
    }
}