use std::io;

use bitpacking::{BitPacker, BitPacker4x};

use crate::{
    disk::{
        codec::{DecodedBlock, decode_block, read_var_u32, skip_varints},
        format::{SKIP_ENTRY_LEN, SkipEntry},
    },
    term_dict::TermMeta,
    types::DocId,
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
    // boxed so that moving a cursor (WAND sorts them every step) copies a pointer, not 2.8 KB
    block: Box<DecodedBlock>,
    slot: usize,
    exhausted: bool,
}

impl<'a> BlockCursor<'a> {
    pub(crate) fn new(bytes: &'a [u8], meta: TermMeta) -> io::Result<Self> {
        let region = TermRegion::split(bytes, meta)?;
        let mut cursor = Self {
            packer: BitPacker4x::new(),
            exhausted: region.block_count == 0,
            region,
            block_index: 0,
            entry: SkipEntry::default(),
            block: Box::default(),
            slot: 0,
        };
        if !cursor.exhausted {
            cursor.load_block(0);
        }
        Ok(cursor)
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
        let entry = SkipEntry::read_at(self.region.skip_table, index)?;
        let bytes = self
            .region
            .blocks
            .get(entry.block_start_offset as usize..)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "block offset past blocks area"))?;
        decode_block(&self.packer, bytes, &entry, &mut self.block)?;
        self.entry = entry;
        self.block_index = index;
        self.slot = 0;
        Ok(())
    }

    #[inline]
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    #[inline]
    pub fn doc_id(&self) -> Option<DocId> {
        (!self.exhausted).then(|| self.block.docs[self.slot])
    }

    /// Term frequency of the current document; 0 when exhausted.
    #[inline]
    pub fn term_freq(&self) -> u32 {
        if self.exhausted { 0 } else { self.block.term_freq[self.slot] }
    }

    /// Weight of the current document; 0 when exhausted.
    #[inline]
    pub fn weight(&self) -> u16 {
        if self.exhausted { 0 } else { self.block.weights[self.slot] }
    }

    /// The skip entry of the block the cursor is in (block bounds for block-max WAND).
    #[inline]
    pub(crate) fn block_entry(&self) -> &SkipEntry {
        &self.entry
    }

    pub fn next(&mut self) {
        if self.exhausted {
            return;
        }
        self.slot += 1;
        if self.slot >= self.block.len {
            self.load_block(self.block_index + 1);
        }
    }

    /// Moves to the first document with id >= `target`. Never moves backwards.
    pub fn advance_to(&mut self, target: DocId) {
        if self.exhausted || self.block.docs[self.slot] >= target {
            return;
        }

        if self.entry.last_doc_id < target {
            // first later block whose last doc reaches target
            let mut low = self.block_index + 1;
            let mut high = self.region.block_count;
            while low < high {
                let mid = low + (high - low) / 2;
                match SkipEntry::last_doc_at(self.region.skip_table, mid) {
                    Ok(last) if last < target => low = mid + 1,
                    Ok(_) => high = mid,
                    Err(_) => {
                        self.exhausted = true;
                        return;
                    }
                }
            }
            if !self.load_block(low) {
                return;
            }
        }

        let remaining = &self.block.docs[self.slot..self.block.len];
        self.slot += remaining.partition_point(|&doc| doc < target);

        // last_doc_id >= target guarantees a hit inside the block; only a corrupt file misses
        if self.slot >= self.block.len {
            self.load_block(self.block_index + 1);
        }
    }

    /// Positions of the current document, appended to `out` (which is cleared first).
    pub fn positions_into(&self, out: &mut Vec<u32>) -> io::Result<()> {
        out.clear();
        if self.exhausted {
            return Ok(());
        }
        let mut at = self.entry.positions_start_offset as usize;
        let before: usize = self.block.term_freq[..self.slot].iter().map(|&tf| tf as usize).sum();
        skip_varints(self.region.positions, &mut at, before)?;

        let mut position = 0u32;
        for _ in 0..self.term_freq() {
            position += read_var_u32(self.region.positions, &mut at)?;
            out.push(position);
        }
        Ok(())
    }

    /// Highlight offsets of the current document, appended to `out` (cleared first). Empty when
    /// the document was indexed without offsets.
    pub fn offsets_into(&self, out: &mut Vec<(u32, u32)>) -> io::Result<()> {
        out.clear();
        if self.exhausted {
            return Ok(());
        }
        let offsets = self.region.offsets;
        let mut at = self.entry.offsets_start_offset as usize;

        let present_at = |at: usize| -> io::Result<bool> {
            offsets
                .get(at)
                .map(|&flag| flag != 0)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing offsets flag"))
        };

        for &tf in &self.block.term_freq[..self.slot] {
            let present = present_at(at)?;
            at += 1;
            if present {
                skip_varints(offsets, &mut at, 2 * tf as usize)?;
            }
        }

        let present = present_at(at)?;
        at += 1;
        if present {
            for _ in 0..self.term_freq() {
                let start = read_var_u32(offsets, &mut at)?;
                let len = read_var_u32(offsets, &mut at)?;
                out.push((start, start + len));
            }
        }
        Ok(())
    }
}