use core_timing::timed;
use std::collections::BTreeMap;
use std::fs;
use std::io::SeekFrom;
use bitpacking::{BitPacker, BitPacker4x};
use std::{
    fs::File,
    io::{self, BufWriter, Seek, Write},
    path::Path,
};

use crate::{array_rows::ArrayRowIndex, disk::{block_cursor::{BlockCursor, TermRegion}, format::{BLOCK_LEN, SkipEntry}}, posting::{DeleteSet, PostingCursor, cursor::TermCursor}};
use crate::document::document::BoolField;
use crate::tmp_path_for;
use crate::{
    disk::{
        codec::{encode_block, push_var_u32},

        format::{SegmentFooter, SegmentHeader},
    },
    numeric_values::NumericFields,
    posting::PostingList,
    segment::ImmutableSegment,
    segment::compute_doc_id_range,
    term_dict::{TermDict, TermMeta},
    types::{DocId, TermKey, XPathId},
};

/// One input's postings for one term: a disk region (copied undecoded when possible) or a
/// list in memory.
pub(crate) enum MergeSource<'a> {
    Disk { bytes: &'a [u8], meta: TermMeta },
    Memory(&'a PostingList),
}
struct BlockWriter {
    packer:BitPacker4x,
    // the block being built
    docs: [u64; 128],
    doc_count:u32,
    term_frequency: [u32; 128],
    weights: [u16; 128],
    max_weight:u16,
    slots: usize,

    // per-block stats, reset in flush()
    block_max_tf: u32,
    block_max_weight: u16,

    block_min_doc_len: u32,    // for the block-max WAND bound
    block_positions_start: u32,
    block_offsets_start: u32,

    // output
    skip_table: Vec<u8>,
    blocks_packed: Vec<u8>,
    positions: Vec<u8>,
    offsets: Vec<u8>,
    block_count:u32,
    last_doc_id: u64,
}
impl BlockWriter {
    fn new() -> Self {
        Self {
            packer: BitPacker4x::new(),
            docs: [0; BLOCK_LEN],
            doc_count:0,
            term_frequency: [0; BLOCK_LEN],
            weights: [0; BLOCK_LEN],
            max_weight:0,
            slots: 0,
            block_max_tf: 0,
            block_max_weight: 0,
            block_min_doc_len: u32::MAX,
            block_positions_start: 0,
            block_offsets_start: 0,
            skip_table: Vec::new(),
            blocks_packed: Vec::with_capacity(64 * 1024),
            positions: Vec::with_capacity(64 * 1024),
            offsets: Vec::new(),
            block_count: 0,
            last_doc_id: 0,
        }
    }

        /// Encodes one term from its sources into this writer's buffers, replacing the previous
    /// term. A disk source is copied as raw bytes when nothing is deleted and the sources follow
    /// each other; otherwise the sources are merged through cursors, dropping deleted docs.
    #[timed(writing_files)]
    fn write_term(
        &mut self,
        sources: &[MergeSource<'_>],
        deleted: &DeleteSet,
        doc_len: impl Fn(DocId) -> Option<u32>,
    ) -> io::Result<()> {
        self.skip_table.clear();
        self.blocks_packed.clear();
        self.positions.clear();
        self.offsets.clear();
        self.block_count = 0;
        self.slots = 0;
        self.last_doc_id = 0;
        self.doc_count = 0;
        self.max_weight = 0;
        self.reset_block_stats();

        // each source's first and last doc, to see whether they follow each other
        let mut ranges = Vec::with_capacity(sources.len());
        for source in sources {
            ranges.push(match source {
                MergeSource::Disk { bytes, meta } => {
                    let region = TermRegion::split(bytes, *meta)?;
                    if region.block_count == 0 {
                        None
                    } else {
                        let first = SkipEntry::read_at(region.skip_table, 0)?.first_doc_id;
                        let last = SkipEntry::read_at(region.skip_table, region.block_count - 1)?
                            .last_doc_id;
                        Some((first, last))
                    }
                }
                MergeSource::Memory(list) => list
                    .items()
                    .first()
                    .zip(list.items().last())
                    .map(|(first, last)| (first.doc_id, last.doc_id)),
            });
        }
        let ordered = ranges
            .iter()
            .flatten()
            .zip(ranges.iter().flatten().skip(1))
            .all(|(before, after)| before.1 < after.0);

        // ordered: each source on its own; overlapping: all of them merged together
        let groups: Vec<Vec<&MergeSource<'_>>> = if ordered {
            sources.iter().map(|source| vec![source]).collect()
        } else {
            vec![sources.iter().collect()]
        };

        let mut positions: Vec<u32> = Vec::new();
        let mut offsets: Vec<(u32, u32)> = Vec::new();
        let mut more_positions: Vec<u32> = Vec::new();
        let mut more_offsets: Vec<(u32, u32)> = Vec::new();

        for group in groups {
            // raw copy: blocks and streams as they are, skip entries shifted
            if let [MergeSource::Disk { bytes, meta }] = group.as_slice()
                && deleted.is_empty()
            {
                let region = TermRegion::split(bytes, *meta)?;
                if region.block_count == 0 {
                    continue;
                }
                self.flush_block();
                let blocks_base = stream_offset(&self.blocks_packed);
                let positions_base = stream_offset(&self.positions);
                let offsets_base = stream_offset(&self.offsets);
                let shift = |value: u32, by: u32| {
                    value.checked_add(by).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "term postings exceed 4 GiB")
                    })
                };
                for index in 0..region.block_count {
                    let mut entry = SkipEntry::read_at(region.skip_table, index)?;
                    entry.block_start_offset = shift(entry.block_start_offset, blocks_base)?;
                    entry.positions_start_offset =
                        shift(entry.positions_start_offset, positions_base)?;
                    entry.offsets_start_offset = shift(entry.offsets_start_offset, offsets_base)?;
                    self.last_doc_id = entry.last_doc_id;
                    entry.write_to(&mut self.skip_table);
                }
                self.blocks_packed.extend_from_slice(region.blocks);
                self.positions.extend_from_slice(region.positions);
                self.offsets.extend_from_slice(region.offsets);
                self.block_count += u32::try_from(region.block_count).unwrap_or(u32::MAX);
                self.doc_count += meta.doc_freq;
                self.max_weight = self.max_weight.max(meta.max_weight);
                continue;
            }

            // decoded merge by doc id
            let mut cursors: Vec<TermCursor<'_>> = Vec::with_capacity(group.len());
            for source in &group {
                cursors.push(match source {
                    MergeSource::Disk { bytes, meta } => {
                        TermCursor::Block(Box::new(BlockCursor::new(bytes, *meta)?))
                    }
                    MergeSource::Memory(list) => TermCursor::Slice(PostingCursor::new(list)),
                });
            }

            while let Some(doc) = cursors.iter().filter_map(TermCursor::doc_id).min() {
                if !deleted.contains(doc) {
                    let mut weight = 0u16;
                    let mut found = 0usize;
                    for cursor in cursors.iter().filter(|cursor| cursor.doc_id() == Some(doc)) {
                        weight = weight.max(cursor.weight());
                        if found == 0 {
                            cursor.positions_into(&mut positions)?;
                            cursor.offsets_into(&mut offsets)?;
                        } else {
                            // the same doc in several sources: merge, as `from_sorted` did
                            cursor.positions_into(&mut more_positions)?;
                            cursor.offsets_into(&mut more_offsets)?;
                            let with_offsets = offsets.len() == positions.len()
                                && more_offsets.len() == more_positions.len();
                            let pad = std::iter::repeat((0, 0));
                            let mut both: Vec<(u32, (u32, u32))> = positions
                                .iter()
                                .copied()
                                .zip(offsets.iter().copied().chain(pad.clone()))
                                .chain(
                                    more_positions
                                        .iter()
                                        .copied()
                                        .zip(more_offsets.iter().copied().chain(pad)),
                                )
                                .collect();
                            both.sort_unstable_by_key(|(position, _)| *position);
                            both.dedup_by_key(|(position, _)| *position);
                            positions = both.iter().map(|(position, _)| *position).collect();
                            offsets = if with_offsets {
                                both.iter().map(|(_, offset)| *offset).collect()
                            } else {
                                Vec::new()
                            };
                        }
                        found += 1;
                    }
                    if !positions.is_empty() {
                        self.push(doc, &positions, &offsets, weight, doc_len(doc));
                    }
                }
                for cursor in &mut cursors {
                    if cursor.doc_id() == Some(doc) {
                        cursor.next();
                    }
                }
            }
        }

        self.flush_block();
        Ok(())
    }

        fn push(
        &mut self,
        doc_id: DocId,
        positions: &[u32],
        offsets: &[(u32, u32)],
        weight: u16,
        doc_len: Option<u32>,
    ) {
        self.doc_count += 1;
        self.max_weight = self.max_weight.max(weight);

        // a gap that does not fit u32 (the jump to array-row ids) starts a new block, whose
        // first doc is stored absolutely in its skip entry
        if self.slots > 0 && doc_id - self.last_doc_id > u64::from(u32::MAX) {
            self.flush_block();
        }

        if self.slots == 0 {
            self.block_positions_start = stream_offset(&self.positions);
            self.block_offsets_start = stream_offset(&self.offsets);
        }

        let tf = u32::try_from(positions.len()).expect("term frequency fits u32");
        let slot = self.slots;
        self.docs[slot] = doc_id;
        self.term_frequency[slot] = tf;
        self.weights[slot] = weight;

        self.block_max_tf = self.block_max_tf.max(tf);
        self.block_max_weight = self.block_max_weight.max(weight);
        // 0 = unknown length: the block bound then falls back to the term bound
        self.block_min_doc_len = self.block_min_doc_len.min(doc_len.unwrap_or(0));

        let mut last_position = 0u32;
        for &position in positions {
            push_var_u32(&mut self.positions, position - last_position);
            last_position = position;
        }

        let has_offsets = !offsets.is_empty();
        debug_assert!(!has_offsets || offsets.len() == positions.len());
        self.offsets.push(u8::from(has_offsets));
        if has_offsets {
            for &(start, end) in offsets {
                push_var_u32(&mut self.offsets, start);
                push_var_u32(&mut self.offsets, end - start);
            }
        }

        self.slots += 1;
        self.last_doc_id = doc_id;

        if self.slots == BLOCK_LEN {
            self.flush_block();
        }
    }

    fn flush_block(&mut self) {
        if self.slots == 0 {
            return;
        }
        let len = self.slots;
        let block_start = stream_offset(&self.blocks_packed);

        encode_block(
            &self.packer,
            &self.docs[..len],
            &self.term_frequency[..len],
            &self.weights[..len],
            &mut self.blocks_packed,
        );

        SkipEntry {
            first_doc_id: self.docs[0],
            last_doc_id: self.docs[len - 1],
            block_start_offset:block_start,
            positions_start_offset: self.block_positions_start,
            offsets_start_offset: self.block_offsets_start,
            max_tf: self.block_max_tf,
            min_doc_len: self.block_min_doc_len,
            max_weight: self.block_max_weight,
            len: u8::try_from(len).expect("block length is at most 128"),
        }
        .write_to(&mut self.skip_table);

        self.block_count += 1;
        self.slots = 0;
        self.reset_block_stats();
    }

    fn reset_block_stats(&mut self) {
        self.block_max_tf = 0;
        self.block_max_weight = 0;
        self.block_min_doc_len = u32::MAX;
    }

    fn total_len(&self) -> usize {
        self.skip_table.len() + self.blocks_packed.len() + self.positions.len() + self.offsets.len()
    }
}

fn stream_offset(buffer: &[u8]) -> u32 {
    u32::try_from(buffer.len()).expect("one term's postings stream exceeds 4 GiB")
}
struct PositionWriter<W> {
    inner: W,
    pos: u64,
}
impl<W: Write> PositionWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, pos: 0 }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}
impl<W: Write> Write for PositionWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl<W: Write> Seek for PositionWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Current(0) => Ok(self.pos),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PositionWriter only reports its position",
            )),
        }
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}
// Streams postings out while grouping terms into one FST per field.
// Terms MUST arrive in ascending (xpath, term)
struct FieldWriter<K> {
    fields: Vec<(XPathId, TermDict)>,
    current: Option<XPathId>,
    entries: Vec<(K, TermMeta)>,
    blocks:BlockWriter,
    term_count: u32,
}

impl<K: AsRef<[u8]>> FieldWriter<K> {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            current: None,
            entries: Vec::new(),
            blocks: BlockWriter::new(),
            term_count: 0,
        }
    }

    // Encodes one term's postings at the current position and records where they
    // landed, closing the previous field's FST when the xpath changes.
     fn push<W: Write + Seek>(
        &mut self,
        out: &mut W,
        xpath: XPathId,
        term: K,
        sources: &[MergeSource<'_>],
        deleted: &DeleteSet,
        doc_len: impl Fn(DocId) -> Option<u32>,
    ) -> io::Result<()> {
        self.blocks.write_term(sources, deleted, doc_len)?;
        // every doc deleted: the term is not written at all
        if self.blocks.doc_count == 0 {
            return Ok(());
        }

        if self.current != Some(xpath) {
            self.close_field()?;
            self.current = Some(xpath);
        }
        let postings_offset = out.stream_position()?;
 

        let blocks = &self.blocks;
        out.write_all(&blocks.skip_table)?;
        out.write_all(&blocks.blocks_packed)?;
        out.write_all(&blocks.positions)?;
        out.write_all(&blocks.offsets)?;

        let to_u32 = |len: usize| {
            u32::try_from(len).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "term postings exceed 4 GiB")
            })
        };

        self.entries.push((
            term,
            TermMeta {
                postings_offset,
                postings_len: to_u32(blocks.total_len())?,
                doc_freq: blocks.doc_count,
                max_weight: blocks.max_weight,
                block_count: blocks.block_count,
                block_len: to_u32(blocks.blocks_packed.len())?,
                positions_len: to_u32(blocks.positions.len())?,
            },
        ));

        self.term_count += 1;
        Ok(())
    }

    // Builds the FST for the field we were accumulating, if any.
    fn close_field(&mut self) -> io::Result<()> {
        if let Some(xpath) = self.current.take() {
            self.fields
                .push((xpath, TermDict::build(self.entries.drain(..))?));
        }

        Ok(())
    }

    fn finish(mut self) -> io::Result<(Vec<(XPathId, TermDict)>, u32)> {
        self.close_field()?;
        Ok((self.fields, self.term_count))
    }
}

fn write_u8(out: &mut impl Write, value: u8) -> io::Result<()> {
    out.write_all(&[value])
}

fn write_u16(out: &mut impl Write, value: u16) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_u32(out: &mut impl Write, value: u32) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_u64(out: &mut impl Write, value: u64) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_header(out: &mut impl Write) -> io::Result<()> {
    let header = SegmentHeader::current();
    out.write_all(&header.magic)?;
    write_u32(out, header.version)
}

fn write_footer(out: &mut impl Write, footer: &SegmentFooter) -> io::Result<()> {
    write_u64(out, footer.doc_lengths_offset)?;
    write_u64(out, footer.doc_lengths_len)?;
    write_u64(out, footer.dictionary_offset)?;
    write_u64(out, footer.dictionary_len)?;
    write_u64(out, footer.numeric_fields_offset)?;
    write_u64(out, footer.numeric_fields_len)?;
    write_u32(out, footer.term_count)?;
    write_u64(out, footer.min_doc_id)?;
    write_u64(out, footer.max_doc_id)?;
    write_u64(out, footer.array_row_index_offset)?;
    write_u64(out, footer.array_row_index_len)?;
    write_u64(out, footer.bool_fields_offset)?;
    write_u64(out, footer.bool_fields_len)
}

fn write_dictionary(out: &mut impl Write, fields: &[(XPathId, TermDict)]) -> io::Result<()> {
    write_u32(out, fields.len() as u32)?;

    for (xpath, dict) in fields {
        write_u32(out, *xpath)?;
        write_u32(out, dict.len() as u32)?;

        let fst = dict.fst_bytes();
        write_u64(out, fst.len() as u64)?;
        out.write_all(fst)?;

        for meta in dict.metas() {
            write_u64(out, meta.postings_offset)?;
            write_u32(out, meta.postings_len)?;
            write_u32(out, meta.doc_freq)?;
            write_u16(out, meta.max_weight)?;
            write_u32(out, meta.block_count)?;
            write_u32(out, meta.block_len)?;
            write_u32(out, meta.positions_len)?;
        }
    }

    Ok(())
}

fn write_numeric_fields(out: &mut impl Write, fields: &NumericFields) -> io::Result<()> {
    write_u32(out, fields.iter().count() as u32)?;

    for (xpath, field) in fields.iter() {
        write_u32(out, xpath)?;

        write_u8(out, field.bkd.kind().to_byte())?;

        // BKD: sorted by packed value (range queries).
        write_u32(out, field.bkd.len() as u32)?;
        for &(packed, doc_id) in field.bkd.points() {
            write_u64(out, packed)?;
            write_u64(out, doc_id)?;
        }

        // DocValues: sorted by doc_id (point lookups).
        write_u32(out, field.doc_values.len() as u32)?;
        for &(doc_id, packed) in field.doc_values.entries() {
            write_u64(out, doc_id)?;
            write_u64(out, packed)?;
        }
    }

    Ok(())
}

fn write_bool_fields(
    out: &mut impl Write,
    fields: &BTreeMap<XPathId, BoolField>,
) -> io::Result<()> {
    write_u32(out, fields.len() as u32)?;
    for (xpath, field) in fields {
        write_u32(out, *xpath)?;
        let mut buf = Vec::new();
        field.true_ids.serialize_into(&mut buf)?;
        write_u64(out, buf.len() as u64)?;
        out.write_all(&buf)?;
        buf.clear();
        field.false_ids.serialize_into(&mut buf)?;
        write_u64(out, buf.len() as u64)?;
        out.write_all(&buf)?;
    }
    Ok(())
}

#[timed(writing_files)]
pub fn write_segment(path: impl AsRef<Path>, segment: &ImmutableSegment) -> io::Result<()> {
    let path = path.as_ref();
    let tmp = tmp_path_for(path);
    let written = File::create(&tmp).and_then(|file| {
        let mut out = PositionWriter::new(BufWriter::with_capacity(1 << 20, file));
        write_segment_to(&mut out, segment)?;
        finish_file(out, &tmp, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[timed(writing_files)]
pub fn write_segment_to<W: Write + Seek>(
    out: &mut W,
    segment: &ImmutableSegment,
) -> io::Result<()> {
    write_header(out)?;

    let mut writer: FieldWriter<&str> = FieldWriter::new();

    let no_deletes = DeleteSet::default();
    let doc_lengths = segment.doc_lengths();
    for (key, postings) in segment.terms() {
        let xpath = key.xpath;
        writer.push(
            out,
            xpath,
            key.term.as_str(),
            &[MergeSource::Memory(postings)],
            &no_deletes,
            |doc| doc_lengths.get(&(doc, xpath)).copied(),
        )?;
    }

    let (fields, term_count) = writer.finish()?;

    let doc_lengths_offset = out.stream_position()?;
    write_doc_lengths(out, segment.doc_lengths())?;
    let doc_lengths_end = out.stream_position()?;

    let dictionary_offset = out.stream_position()?;
    write_dictionary(out, &fields)?;
    let dictionary_end = out.stream_position()?;

    let numeric_fields_offset = out.stream_position()?;
    write_numeric_fields(out, segment.numeric_fields())?;
    let numeric_fields_end = out.stream_position()?;
    let doc_id_range = segment.doc_range().unwrap_or((0, 0));

    let array_row_index_offset = out.stream_position()?;
    write_array_row_index(out, segment.array_row_index())?;
    let array_row_index_end = out.stream_position()?;

    let bool_fields_offset = out.stream_position()?;
    write_bool_fields(out, segment.bool_fields())?;
    let bool_fields_end = out.stream_position()?;

    let footer = SegmentFooter {
        doc_lengths_offset,
        doc_lengths_len: doc_lengths_end - doc_lengths_offset,
        dictionary_offset,
        dictionary_len: dictionary_end - dictionary_offset,
        numeric_fields_offset,
        numeric_fields_len: numeric_fields_end - numeric_fields_offset,
        array_row_index_offset,
        array_row_index_len: array_row_index_end - array_row_index_offset,
        bool_fields_offset,
        bool_fields_len: bool_fields_end - bool_fields_offset,
        term_count,
        min_doc_id: doc_id_range.0,
        max_doc_id: doc_id_range.1,
    };

    write_footer(out, &footer)?;
    let total_bytes = out.stream_position()?;
    core_timing::add_bytes("writing_files", "write_segment_to", file!(), total_bytes);

    Ok(())
}

#[timed(writing_files)]
fn write_doc_lengths(
    out: &mut impl Write,
    doc_lengths: &BTreeMap<(DocId, XPathId), u32>,
) -> io::Result<()> {
    write_u32(out, doc_lengths.len() as u32)?;

    for (&(doc_id, xpath), &len) in doc_lengths {
        //maybe change to DocId
        write_u64(out, doc_id)?;
        write_u32(out, xpath)?;
        write_u32(out, len)?;
    }

    Ok(())
}

fn write_array_row_index(out: &mut impl Write, idx: &ArrayRowIndex) -> io::Result<()> {
    write_u64(out, idx.base().raw())?;
    write_u32(out, idx.len() as u32)?;
    for ((&doc, &parent), &element) in idx
        .row_to_doc()
        .iter()
        .zip(idx.parent_rows().iter())
        .zip(idx.element_indices().iter())
    {
        write_u64(out, doc)?;
        write_u64(out, parent.raw())?;
        write_u32(out, element)?;
    }
    Ok(())
}


#[timed(writing_files)]
 pub(crate) fn write_merged_segment<'s>(
    path: impl AsRef<Path>,
    terms: impl Iterator<Item = (TermKey, Vec<MergeSource<'s>>)>,
    deleted: &DeleteSet,
    doc_lengths: &BTreeMap<(DocId, XPathId), u32>,
    numeric_fields: &NumericFields,
    array_row_index: &ArrayRowIndex,
    bool_fields: &BTreeMap<XPathId, BoolField>,
) -> io::Result<()> {
    let path = path.as_ref();
    let tmp = tmp_path_for(path);
    let written = File::create(&tmp).and_then(|file| {
        let mut out = PositionWriter::new(BufWriter::with_capacity(1 << 20, file));
        write_merged_segment_to(
            &mut out,
            terms,
            deleted,
            doc_lengths,
            numeric_fields,
            array_row_index,
            bool_fields,
        )?;
        finish_file(out, &tmp, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}
#[timed(writing_files)]
pub fn write_merged_segment_to<'s, W: Write + Seek>(
    out: &mut W,
    terms: impl Iterator<Item = (TermKey, Vec<MergeSource<'s>>)>,
    deleted: &DeleteSet,
    doc_lengths: &BTreeMap<(DocId, XPathId), u32>,
    numeric_fields: &NumericFields,
    array_row_index: &ArrayRowIndex,
    bool_fields: &BTreeMap<XPathId, BoolField>,
) -> io::Result<()> {
    write_header(out)?;

    let mut writer: FieldWriter<String> = FieldWriter::new();

    for (key, sources) in terms {
        let xpath = key.xpath;
        writer.push(out, xpath, key.term, &sources, deleted, |doc| {
            doc_lengths.get(&(doc, xpath)).copied()
        })?;
    }

    let (fields, term_count) = writer.finish()?;

    let doc_lengths_offset = out.stream_position()?;
    write_doc_lengths(out, doc_lengths)?;
    let doc_lengths_end = out.stream_position()?;

    let dictionary_offset = out.stream_position()?;
    write_dictionary(out, &fields)?;
    let dictionary_end = out.stream_position()?;

    let numeric_fields_offset = out.stream_position()?;
    write_numeric_fields(out, numeric_fields)?;
    let numeric_fields_end = out.stream_position()?;

    let doc_id_range = compute_doc_id_range(doc_lengths, numeric_fields).unwrap_or((0, 0));
    let array_row_index_offset = out.stream_position()?;
    write_array_row_index(out, array_row_index)?;
    let array_row_index_end = out.stream_position()?;

    let bool_fields_offset = out.stream_position()?;
    write_bool_fields(out, bool_fields)?;
    let bool_fields_end = out.stream_position()?;

    let footer = SegmentFooter {
        doc_lengths_offset,
        doc_lengths_len: doc_lengths_end - doc_lengths_offset,
        dictionary_offset,
        dictionary_len: dictionary_end - dictionary_offset,
        numeric_fields_offset,
        numeric_fields_len: numeric_fields_end - numeric_fields_offset,
        array_row_index_offset,
        array_row_index_len: array_row_index_end - array_row_index_offset,
        bool_fields_offset,
        bool_fields_len: bool_fields_end - bool_fields_offset,
        term_count,
        min_doc_id: doc_id_range.0,
        max_doc_id: doc_id_range.1,
    };

    write_footer(out, &footer)
}

fn finish_file(out: PositionWriter<BufWriter<File>>, tmp: &Path, path: &Path) -> io::Result<()> {
    let file = out.into_inner().into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(dir)?.sync_all()
}
