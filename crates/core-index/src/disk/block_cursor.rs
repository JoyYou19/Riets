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
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);

        let block_count = meta.block_count as usize;
        let skip_len = block_count
            .checked_mul(SKIP_ENTRY_LEN)
            .ok_or_else(|| invalid("skip table size overflow"))?;
        let blocks_end = skip_len
            .checked_add(meta.block_len as usize)
            .ok_or_else(|| invalid("blocks size overflow"))?;
        let positions_end = blocks_end
            .checked_add(meta.positions_len as usize)
            .ok_or_else(|| invalid("positions size overflow"))?;
        if positions_end > bytes.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "term region truncated"));
        }

        Ok(Self {
            skip_table: &bytes[..skip_len],
            blocks: &bytes[skip_len..blocks_end],
            positions: &bytes[blocks_end..positions_end],
            offsets: &bytes[positions_end..],
            block_count,
        })
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
        let bytes = self.region.blocks
            .get(entry.block_start_offset as usize..)
            .ok_or_else(||
                io::Error::new(io::ErrorKind::InvalidData, "block offset past blocks area")
            )?;
        let block = self.block.get_or_insert_with(Box::default);
        decode_block(&self.packer, bytes, &entry, block)?;
        self.entry = entry;
        self.block_index = index;
        self.slot = 0;
        self.loaded = true;
        Ok(())
    }

    #[inline]
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    #[inline]
    pub fn doc_id(&self) -> Option<DocId> {
        if self.exhausted {
            None
        } else if !self.loaded {
            Some(self.entry.first_doc_id) // the cursor is on block 0's first document
        } else {
            self.block.as_ref().map(|b| b.docs[self.slot])
        }
    }

    #[inline]
    pub fn term_freq(&mut self) -> u32 {
        self.ensure_loaded();
        if self.exhausted {
            0
        } else {
            self.block.as_ref().map_or(0, |b| b.term_freq[self.slot])
        }
    }

    /// Weight of the current document; 0 when exhausted.
    #[inline]
    pub fn weight(&mut self) -> u16 {
        self.ensure_loaded();
        if self.exhausted {
            0
        } else {
            self.block.as_ref().map_or(0, |b| b.weights[self.slot])
        }
    }

    /// The skip entry of the block the cursor is in (block bounds for block-max WAND).
    #[inline]
    pub fn block_entry(&self) -> &SkipEntry {
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
        if self.slot >= self.block.as_ref().unwrap().len {
            //unwrap safe
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
            self.block.as_ref().unwrap().docs[self.slot]
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

        let block = self.block.as_ref().unwrap(); // loaded on every path above
        let remaining = &block.docs[self.slot..block.len];
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
        while
            self.position_index < self.position_cache.len() &&
            position_start(self.position_cache[self.position_index]) < target
        {
            self.position_index += 1;
        }
        Ok(self.position_index < self.position_cache.len())
    }

    /// The position `advance_to_position` is currently on, if any.
    pub fn current_position(&self) -> Option<u32> {
        self.position_cache.get(self.position_index).copied()
    }

    fn ensure_positions_decoded(&mut self) -> io::Result<()> {
    if !self.position_cache.is_empty() || self.term_freq() == 0 {
        return Ok(()); // term_freq() also loads the block
    }
    if self.exhausted {
        return Ok(());
    }
    let block = self.block.as_ref().unwrap(); // loaded by term_freq()
    let tf = block.term_freq[self.slot];
    let before: usize = block.term_freq[..self.slot].iter().map(|&t| t as usize).sum();

    let mut at = self.entry.positions_start_offset as usize;
    skip_varints(self.region.positions, &mut at, before)?;

    let mut position = 0u32;
    for _ in 0..tf {
        position += read_var_u32(self.region.positions, &mut at)?;
        self.position_cache.push(position);
    }
    Ok(())
}

    fn decode_current_positions(&self, out: &mut Vec<u32>) -> io::Result<()> {
    out.clear();
    let Some(block) = self.block.as_ref().filter(|_| !self.exhausted && self.loaded) else {
        return Ok(());
    };
    let tf = block.term_freq[self.slot];
    let before: usize = block.term_freq[..self.slot].iter().map(|&t| t as usize).sum();

    let mut at = self.entry.positions_start_offset as usize;
    skip_varints(self.region.positions, &mut at, before)?;

    let mut position = 0u32;
    for _ in 0..tf {
        position += read_var_u32(self.region.positions, &mut at)?;
        out.push(position);
    }
    Ok(())
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
    let mut at = self.entry.offsets_start_offset as usize;

    let present_at = |at: usize| -> io::Result<bool> {
        offsets
            .get(at)
            .map(|&flag| flag != 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing offsets flag"))
    };

    for &tf in &block.term_freq[..self.slot] {
        let present = present_at(at)?;
        at += 1;
        if present {
            skip_varints(offsets, &mut at, 2 * (tf as usize))?;
        }
    }

    let present = present_at(at)?;
    at += 1;
    if present {
        for _ in 0..block.term_freq[self.slot] {
            let start = read_var_u32(offsets, &mut at)?;
            let len = read_var_u32(offsets, &mut at)?;
            out.push((start, start + len));
        }
    }
    Ok(())
}
}
