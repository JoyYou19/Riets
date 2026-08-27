use std::{
    fs::File,
    io::{self, BufWriter, Seek, Write},
    path::Path,
};

use core_timing::timed;

use crate::{
    disk::{
        codec::{push_var_u16, push_var_u32, push_var_u64},
        format::{SegmentFooter, SegmentHeader, TermEntry},
    },
    posting::PostingList,
    segment::ImmutableSegment,
    types::{DocId, TermKey, XPathId},
};

/*
* Writes an ImmutableSegment to disk
*/

fn trace_segment_writer() -> bool {
    std::env::var_os("CORELAMO_TRACE_SEGMENT_WRITER").is_some()
}

// fn write_u16(out: &mut impl Write, value: u16) -> io::Result<()> {
//     out.write_all(&value.to_le_bytes())
// }

fn write_u32(out: &mut impl Write, value: u32) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_u64(out: &mut impl Write, value: u64) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_string(out: &mut impl Write, value: &str) -> io::Result<()> {
    write_u32(out, value.len() as u32)?;
    out.write_all(value.as_bytes())
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
    write_u32(out, footer.term_count)
}

// fn write_posting_list(out: &mut impl Write, list: &PostingList) -> io::Result<()> {
//     let mut last_doc_id = 0u64;
//
//     for posting in list.items() {
//         let doc_delta = posting.doc_id - last_doc_id;
//         last_doc_id = posting.doc_id;
//
//         write_var_u64(out, doc_delta)?;
//         write_var_u16(out, posting.weight)?;
//         write_var_u32(out, posting.positions.len() as u32)?;
//
//         let mut last_position = 0u32;
//
//         for &position in &posting.positions {
//             let position_delta = position - last_position;
//             last_position = position;
//             write_var_u32(out, position_delta)?;
//         }
//     }
//
//     Ok(())
// }

fn write_dictionary(out: &mut impl Write, entries: &[TermEntry]) -> io::Result<()> {
    write_u32(out, entries.len() as u32)?;

    for entry in entries {
        write_string(out, &entry.term)?;
        write_u32(out, entry.xpath)?;
        write_u64(out, entry.postings_offset)?;
        write_u32(out, entry.postings_len)?;
        write_u32(out, entry.doc_freq)?;
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
    let trace = trace_segment_writer();
    let total_started = std::time::Instant::now();

    write_header(out)?;

    if trace {
        //  tracing.trace!(time=?started.elapsed(),"segment writer: header took");
    }

    let started = std::time::Instant::now();

    let mut dictionary = Vec::new();
    let mut posting_lists = 0usize;
    let mut postings_total = 0usize;
    let mut positions_total = 0usize;
    let mut postings_buf = Vec::with_capacity(64 * 1024);

    for (key, postings) in segment.terms() {
        postings_buf.clear();

        let postings_offset = out.stream_position()?;

        encode_posting_list(&mut postings_buf, postings);
        out.write_all(&postings_buf)?;

        let postings_len = postings_buf.len() as u32;

        posting_lists += 1;
        postings_total += postings.len();
        positions_total += postings
            .items()
            .iter()
            .map(|posting| posting.positions.len())
            .sum::<usize>();

        dictionary.push(TermEntry {
            term: key.term.clone(),
            xpath: key.xpath,
            postings_offset,
            postings_len,
            doc_freq: postings.len() as u32,
        });
    }

    if trace {
        tracing::trace!(
            posting_lists=%posting_lists,
            posting_total=%postings_total,
            positions_total=%positions_total,
            time=?started.elapsed(),
            "segment writer wrote postings",
        );
    }

    let doc_lengths_offset = out.stream_position()?;
    write_doc_lengths(out, segment.doc_lengths())?;
    let doc_lengths_end = out.stream_position()?;

    let started = std::time::Instant::now();

    let dictionary_offset = out.stream_position()?;
    write_dictionary(out, &dictionary)?;
    let dictionary_end = out.stream_position()?;

    if trace {
        tracing::trace!(
            time=?started.elapsed(),
            dictionary=%dictionary.len(),
            "segment writer wrote in dictionary",
        );
    }

    let started = std::time::Instant::now();

    let footer = SegmentFooter {
        doc_lengths_offset,
        doc_lengths_len: doc_lengths_end - doc_lengths_offset,
        dictionary_offset,
        dictionary_len: dictionary_end - dictionary_offset,
        term_count: dictionary.len() as u32,
    };

    write_footer(out, &footer)?;

    if trace {
        tracing::trace!(
            time=?started.elapsed(),
            total=?total_started.elapsed(),
            "segment writer wrote footer"
        );
    }

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
) -> io::Result<()> {
    let file = File::create(path)?;
    let mut out = BufWriter::new(file);
    write_merged_segment_to(&mut out, terms, doc_lengths)?;
    out.flush()
}

#[timed(writing_files)]
pub fn write_merged_segment_to<W: Write + Seek>(
    out: &mut W,
    terms: impl Iterator<Item = (TermKey, PostingList)>,
    doc_lengths: &std::collections::BTreeMap<(DocId, XPathId), u32>,
) -> io::Result<()> {
    write_header(out)?;

    let mut dictionary = Vec::new();
    let mut postings_buf = Vec::with_capacity(64 * 1024);

    for (key, postings) in terms {
        postings_buf.clear();

        let postings_offset = out.stream_position()?;
        encode_posting_list(&mut postings_buf, &postings);
        out.write_all(&postings_buf)?;
        let postings_len = postings_buf.len() as u32;

        dictionary.push(TermEntry {
            term: key.term,
            xpath: key.xpath,
            postings_offset,
            postings_len,
            doc_freq: postings.len() as u32,
        });
    }

    let doc_lengths_offset = out.stream_position()?;
    write_doc_lengths(out, doc_lengths)?;
    let doc_lengths_end = out.stream_position()?;

    let dictionary_offset = out.stream_position()?;
    write_dictionary(out, &dictionary)?;
    let dictionary_end = out.stream_position()?;

    let footer = SegmentFooter {
        doc_lengths_offset,
        doc_lengths_len: doc_lengths_end - doc_lengths_offset,
        dictionary_offset,
        dictionary_len: dictionary_end - dictionary_offset,
        term_count: dictionary.len() as u32,
    };

    write_footer(out, &footer)
}
