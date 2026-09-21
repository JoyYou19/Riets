use std::{
    fs::File,
    io::{self, BufWriter, Seek, Write},
    path::Path,
};

use core_timing::timed;

use crate::{
    disk::{
        codec::{push_var_u16, push_var_u32, push_var_u64},
        format::{SegmentFooter, SegmentHeader},
    },
    numeric_columns::{NumericColumns, NumericValue},
    posting::PostingList,
    segment::ImmutableSegment,
    term_dict::{TermDict, TermMeta},
    types::{DocId, TermKey, XPathId},
};

/*
* Writes an ImmutableSegment to disk
*/

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
    write_u64(out, footer.columns_offset)?;
    write_u64(out, footer.columns_len)?;
    write_u32(out, footer.term_count)
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
        }
    }

    Ok(())
}

fn write_columns(out: &mut impl Write, columns: &NumericColumns) -> io::Result<()> {
    write_u32(out, columns.iter().count() as u32)?;

    for (xpath, column) in columns.iter() {
        write_u32(out, xpath)?;
        write_u32(out, column.len() as u32)?;

        for (value, doc_id) in column.entries() {
            match value {
                NumericValue::Int(v) => {
                    write_u8(out, 0)?;
                    write_u64(out, v as u64)?;
                }
                NumericValue::Float(v) => {
                    write_u8(out, 1)?;
                    write_u64(out, v.to_bits())?;
                }
            }
            write_u64(out, doc_id)?;
        }
    }

    Ok(())
}

#[timed(writing_files)]
pub fn write_segment(path: impl AsRef<Path>, segment: &ImmutableSegment) -> io::Result<()> {
    let file = File::create(path)?;
    let mut out = BufWriter::new(file);
    write_segment_to(&mut out, segment)?;
    out.flush()
}

#[timed(writing_files)]
pub fn write_segment_to<W: Write + Seek>(
    out: &mut W,
    segment: &ImmutableSegment,
) -> io::Result<()> {
    write_header(out)?;

    let mut writer: FieldWriter<&str> = FieldWriter::new();

    for (key, postings) in segment.terms() {
        writer.push(out, key.xpath, key.term.as_str(), postings)?;
    }

    let (fields, term_count) = writer.finish()?;

    let doc_lengths_offset = out.stream_position()?;
    write_doc_lengths(out, segment.doc_lengths())?;
    let doc_lengths_end = out.stream_position()?;

    let dictionary_offset = out.stream_position()?;
    write_dictionary(out, &fields)?;
    let dictionary_end = out.stream_position()?;

    let columns_offset = out.stream_position()?;
    write_columns(out, segment.columns())?;
    let columns_end = out.stream_position()?;

    let footer = SegmentFooter {
        doc_lengths_offset,
        doc_lengths_len: doc_lengths_end - doc_lengths_offset,
        dictionary_offset,
        dictionary_len: dictionary_end - dictionary_offset,
        columns_offset,
        columns_len: columns_end - columns_offset,
        term_count,
    };

    write_footer(out, &footer)?;
    let total_bytes = out.stream_position()?;
    core_timing::add_bytes("writing_files", "write_segment_to", file!(), total_bytes);

    Ok(())
}

#[timed(writing_files)]
fn write_doc_lengths(
    out: &mut impl Write,
    doc_lengths: &std::collections::BTreeMap<(DocId, XPathId), u32>,
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

#[timed(writing_files)]
fn encode_posting_list(out: &mut Vec<u8>, list: &PostingList) {
    let mut last_doc_id = 0u64;

    for posting in list.items() {
        let doc_delta = posting.doc_id - last_doc_id;
        last_doc_id = posting.doc_id;

        push_var_u64(out, doc_delta);
        push_var_u16(out, posting.weight);
        push_var_u32(out, posting.positions.len() as u32);

        let mut last_position = 0u32;

        for &position in &posting.positions {
            let position_delta = position - last_position;
            last_position = position;
            push_var_u32(out, position_delta);
        }
    }
}

#[timed(writing_files)]
pub fn write_merged_segment(
    path: impl AsRef<Path>,
    terms: impl Iterator<Item = (TermKey, PostingList)>,
    doc_lengths: &std::collections::BTreeMap<(DocId, XPathId), u32>,
    columns: &NumericColumns,
) -> io::Result<()> {
    let file = File::create(path)?;
    let mut out = BufWriter::new(file);
    write_merged_segment_to(&mut out, terms, doc_lengths, columns)?;
    out.flush()
}

#[timed(writing_files)]
pub fn write_merged_segment_to<W: Write + Seek>(
    out: &mut W,
    terms: impl Iterator<Item = (TermKey, PostingList)>,
    doc_lengths: &std::collections::BTreeMap<(DocId, XPathId), u32>,
    columns: &NumericColumns,
) -> io::Result<()> {
    write_header(out)?;

    let mut writer: FieldWriter<String> = FieldWriter::new();

    for (key, postings) in terms {
        let xpath = key.xpath;
        writer.push(out, xpath, key.term, &postings)?;
    }

    let (fields, term_count) = writer.finish()?;

    let doc_lengths_offset = out.stream_position()?;
    write_doc_lengths(out, doc_lengths)?;
    let doc_lengths_end = out.stream_position()?;

    let dictionary_offset = out.stream_position()?;
    write_dictionary(out, &fields)?;
    let dictionary_end = out.stream_position()?;

    let columns_offset = out.stream_position()?;
    write_columns(out, columns)?;
    let columns_end = out.stream_position()?;

    let footer = SegmentFooter {
        doc_lengths_offset,
        doc_lengths_len: doc_lengths_end - doc_lengths_offset,
        dictionary_offset,
        dictionary_len: dictionary_end - dictionary_offset,
        columns_offset,
        columns_len: columns_end - columns_offset,
        term_count,
    };

    write_footer(out, &footer)
}

// Streams postings out while grouping terms into one FST per field.
// Terms MUST arrive in ascending (xpath, term)
struct FieldWriter<K> {
    fields: Vec<(XPathId, TermDict)>,
    current: Option<XPathId>,
    entries: Vec<(K, TermMeta)>,
    postings_buf: Vec<u8>,
    term_count: u32,
}

impl<K: AsRef<[u8]>> FieldWriter<K> {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            current: None,
            entries: Vec::new(),
            postings_buf: Vec::with_capacity(64 * 1024),
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
    ) -> io::Result<()> {
        if self.current != Some(xpath) {
            self.close_field()?;
            self.current = Some(xpath);
        }

        self.postings_buf.clear();

        let postings_offset = out.stream_position()?;
        encode_posting_list(&mut self.postings_buf, postings);
        out.write_all(&self.postings_buf)?;

        self.entries.push((
            term,
            TermMeta {
                postings_offset,
                postings_len: self.postings_buf.len() as u32,
                doc_freq: postings.len() as u32,
                max_weight: postings.max_weight(),
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
