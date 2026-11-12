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

use crate::{array_rows::ArrayRowIndex, disk::format::{BLOCK_LEN, SkipEntry}, posting::Posting};
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

struct BlockWriter {
    packer:BitPacker4x,
    // the block being built
    docs: [u64; 128],
    term_frequency: [u32; 128],
    weights: [u16; 128],
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
            term_frequency: [0; BLOCK_LEN],
            weights: [0; BLOCK_LEN],
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

    /// Encodes `list` into this writer's buffers, replacing the previous term.
    /// `doc_len` gives a document's length in this term's field.
    #[timed(writing_files)]
    fn write_term(&mut self, list: &PostingList, doc_len: impl Fn(DocId) -> Option<u32>) {
        self.skip_table.clear();
        self.blocks_packed.clear();
        self.positions.clear();
        self.offsets.clear();
        self.block_count = 0;
        self.slots = 0;
        self.last_doc_id = 0;
        self.reset_block_stats();

        for posting in list.items() {
            self.push(posting, doc_len(posting.doc_id));
        }
        self.flush_block();
    }

    fn push(&mut self, posting: &Posting, doc_len: Option<u32>) {
        let doc_id = posting.doc_id;

        // a gap that does not fit u32 (the jump to array-row ids) starts a new block, whose
        // first doc is stored absolutely in its skip entry
        if self.slots > 0 && doc_id - self.last_doc_id > u64::from(u32::MAX) {
            self.flush_block();
        }

        if self.slots == 0 {
            self.block_positions_start = stream_offset(&self.positions);
            self.block_offsets_start = stream_offset(&self.offsets);
        }

        let tf = u32::try_from(posting.positions.len()).expect("term frequency fits u32");
        let slot = self.slots;
        self.docs[slot] = doc_id;
        self.term_frequency[slot] = tf;
        self.weights[slot] = posting.weight;

        self.block_max_tf = self.block_max_tf.max(tf);
        self.block_max_weight = self.block_max_weight.max(posting.weight);
        // 0 = unknown length: the block bound then falls back to the term bound
        self.block_min_doc_len = self.block_min_doc_len.min(doc_len.unwrap_or(0));

        let mut last_position = 0u32;
        for &position in &posting.positions {
            push_var_u32(&mut self.positions, position - last_position);
            last_position = position;
        }

        let has_offsets = !posting.offsets.is_empty();
        debug_assert!(!has_offsets || posting.offsets.len() == posting.positions.len());
        self.offsets.push(u8::from(has_offsets));
        if has_offsets {
            for &(start, end) in &posting.offsets {
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
        postings: &PostingList,
        doc_len: impl Fn(DocId) -> Option<u32>,
    ) -> io::Result<()> {
        if self.current != Some(xpath) {
            self.close_field()?;
            self.current = Some(xpath);
        }

        let postings_offset = out.stream_position()?;
        self.blocks.write_term(postings, doc_len);

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
                doc_freq: to_u32(postings.len())?,
                max_weight: postings.max_weight(),
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

    let doc_lengths = segment.doc_lengths();
    for (key, postings) in segment.terms() {
        let xpath = key.xpath;
        writer.push(out, xpath, key.term.as_str(), postings, |doc| {
            doc_lengths.get(&(doc, xpath)).copied()
        })?;
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
pub fn write_merged_segment(
    path: impl AsRef<Path>,
    terms: impl Iterator<Item = (TermKey, PostingList)>,
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
pub fn write_merged_segment_to<W: Write + Seek>(
    out: &mut W,
    terms: impl Iterator<Item = (TermKey, PostingList)>,
    doc_lengths: &BTreeMap<(DocId, XPathId), u32>,
    numeric_fields: &NumericFields,
    array_row_index: &ArrayRowIndex,
    bool_fields: &BTreeMap<XPathId, BoolField>,
) -> io::Result<()> {
    write_header(out)?;

    let mut writer: FieldWriter<String> = FieldWriter::new();

    for (key, postings) in terms {
        let xpath = key.xpath;
        writer.push(out, xpath, key.term, &postings,|doc|{
            doc_lengths.get(&(doc,xpath)).copied()
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
