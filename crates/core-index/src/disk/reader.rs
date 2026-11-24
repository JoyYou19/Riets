//WARN: Valter, luudzu piedod es atvicaju es centos Kristianu aptureet - Normunds

use std::{ collections::BTreeMap, io, path::Path };

use bitpacking::{ BitPacker, BitPacker4x };
use core_timing::timed;
use memmap2::Mmap;
use roaring::RoaringTreemap;

use crate::{
    array_rows::{ ArrayRowIndex, NO_PARENT_ROW },
    bkd::Bkd,
    disk::{
        block_cursor::BlockCursor,
        codec::{ DecodedBlock, decode_block, read_var_u32 },
        format::{
            FOOTER_LEN,
            HEADER_LEN,
            MAGIC,
            SKIP_ENTRY_LEN,
            SegmentFooter,
            SkipEntry,
            VERSION,
            array_at,
        },
        writer::MergeSource,
    },
    document::indexed::BoolField,
    document_values::DocValues,
    fuzzy::{ FuzzyExpansion, FuzzyOptions },
    numeric_values::{ NumericBound, NumericField, NumericFields, NumericKind, NumericValue },
    posting::{ Positions, Posting, PostingList, cursor::TermCursor, item::Offsets },
    search::{ SearchIndex, SearchNumeric, SearchStats, TermCursorInfo, TermPostings },
    segment::build_field_stats,
    term_dict::{ TERM_META_LEN, TermDict, TermDictionary, TermMeta },
    types::{ ArrayRowId, DocId, FieldStats, TermKey, XPathId },
    wildcard::MAX_PATTERN_TERMS,
};

// Read only disk segment.
// Segmetn file layout is roughly [header][posting bytes][dictionary bytes][footer]
// the whole file is MMAPED we are cool 🤘
pub struct DiskSegment {
    mmap: Mmap,
    dictionary: TermDictionary,
    doc_lengths: DocLengths,
    field_stats: BTreeMap<XPathId, FieldStats>,
    numeric_fields: NumericFields,
    bool_fields: BTreeMap<XPathId, BoolField>,
    doc_id_range: Option<(DocId, DocId)>,
    array_row_index: ArrayRowIndex,
}

impl SearchStats for DiskSegment {
    fn doc_count(&self, xpath: XPathId) -> u64 {
        self.field_stats.get(&xpath).map_or(0, |s| s.doc_count)
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        self.field_stats.get(&xpath).map_or(0, |s| s.total_doc_len)
    }

    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        self.doc_lengths.get(doc_id, xpath)
    }

    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.doc_id_range
    }
}

impl SearchNumeric for DiskSegment {
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>
    ) -> RoaringTreemap {
        self.numeric_fields.range(xpath, lo, hi)
    }

    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.numeric_fields.get(xpath, doc_id)
    }

    fn numeric_top(
        &self,
        xpath: XPathId,
        k: usize,
        descending: bool
    ) -> Vec<(DocId, NumericValue)> {
        self.numeric_fields.top(xpath, k, descending)
    }

    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>) {
        self.numeric_fields.bounds(xpath)
    }
}

impl DiskSegment {
    #[timed(disk_io)]
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: segment files are written under a temporary name, renamed into place and
        // never modified after; a replacement is a new inode, so the mapping cannot change
        // underneath us.
        let mmap = unsafe { Mmap::map(&file)? };

        validate_header(&mmap)?;

        let footer = read_footer(&mmap)?;
        validate_footer(&mmap, &footer)?;

        let doc_lengths = read_doc_lengths(&mmap, &footer)?;
        let field_stats = build_field_stats(doc_lengths.iter());

        let dictionary = read_term_dictionary(&mmap, &footer)?;
        let numeric_fields = read_numeric_fields(&mmap, &footer)?;

        let bool_fields = read_bool_points(&mmap, &footer)?;

        let doc_id_range = if footer.term_count == 0 && numeric_fields.is_empty() {
            None
        } else {
            Some((footer.min_doc_id, footer.max_doc_id))
        };
        let array_row_index = read_array_row_index(&mmap, &footer)?;
        Ok(Self {
            mmap,
            dictionary,
            doc_lengths,
            field_stats,
            numeric_fields,
            bool_fields,
            doc_id_range,
            array_row_index,
        })
    }

    #[must_use]
    pub const fn doc_lengths(&self) -> &DocLengths {
        &self.doc_lengths
    }
    #[must_use]
    pub const fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.doc_id_range
    }

    #[must_use]
    pub const fn array_row_index(&self) -> &ArrayRowIndex {
        &self.array_row_index
    }

    #[must_use]
    pub const fn numeric_fields(&self) -> &NumericFields {
        &self.numeric_fields
    }

    #[must_use]
    pub const fn bool_fields(&self) -> &BTreeMap<XPathId, BoolField> {
        &self.bool_fields
    }

    //bro yo zis so good function //now even better
    pub(crate) fn iter_terms(&self) -> impl Iterator<Item = (TermKey, MergeSource<'_>)> + '_ {
        let this = self;
        self.dictionary.fields().flat_map(move |(xpath, dict)| {
            dict.entries()
                .into_iter()
                .filter_map(move |(term, meta)| {
                    let bytes = this.term_bytes(meta)?;
                    Some((TermKey::new(term, xpath), MergeSource::Disk { bytes, meta }))
                })
        })
    }
    //Decodes the postings and offsets
    #[timed(search)]
    fn read_postings(&self, meta: TermMeta) -> PostingList {
        let Some(bytes) = self.term_bytes(meta) else {
            return PostingList::default();
        };

        read_posting_list(bytes, meta).unwrap_or_default()
    }

    // Reads postings the same way as `read_postings`, but into a caller's buffer
    fn read_postings_into(&self, meta: TermMeta, out: &mut Vec<Posting>) {
        let Some(bytes) = self.term_bytes(meta) else {
            return;
        };

        drop(read_posting_list_into(bytes, meta, out));
    }

    /// The bytes of one term's region, or `None` when the meta points outside the file.
    fn term_bytes(&self, meta: TermMeta) -> Option<&[u8]> {
        let start = usize::try_from(meta.postings_offset).ok()?;
        let end = start.checked_add(meta.postings_len as usize)?;
        self.mmap.get(start..end)
    }
}

//gramatu tarps:
fn read_posting_list_into(bytes: &[u8], meta: TermMeta, out: &mut Vec<Posting>) -> io::Result<()> {
    //duplicate code
    let block_count = meta.block_count as usize;
    let skip_len = block_count
        .checked_mul(SKIP_ENTRY_LEN)
        .ok_or_else(|| invalid("skip table size overflow"))?;
    let truncated = || io::Error::new(io::ErrorKind::UnexpectedEof, "term region truncated");
    let (skip_table, rest) = bytes.split_at_checked(skip_len).ok_or_else(truncated)?;
    let (blocks, rest) = rest.split_at_checked(meta.block_len as usize).ok_or_else(truncated)?;
    let (positions, offsets) = rest
        .split_at_checked(meta.positions_len as usize)
        .ok_or_else(truncated)?;

    let packer = BitPacker4x::new();
    let mut block = DecodedBlock::default();
    let mut position_at = 0usize;
    let mut offset_at = 0usize;

    out.reserve(meta.doc_freq as usize);

    for index in 0..block_count {
        let entry = SkipEntry::read_at(skip_table, index)?;
        let start = entry.block_start_offset as usize;
        let block_bytes = blocks
            .get(start..)
            .ok_or_else(|| invalid("block offset past blocks area"))?;
        decode_block(packer, block_bytes, &entry, &mut block)?;

        for ((&doc, &term_freq), &weight) in block.docs
            .iter()
            .zip(&block.term_freq)
            .zip(&block.weights)
            .take(block.len) {
            let tf = term_freq as usize;

            let mut doc_positions = Positions::with_capacity(tf);
            let mut last_position = 0u32;
            for _ in 0..tf {
                last_position += read_var_u32(positions, &mut position_at)?;
                doc_positions.push(last_position);
            }

            let present =
                *offsets
                    .get(offset_at)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "missing offsets flag")
                    })? != 0;
            offset_at += 1;

            let mut doc_offsets = Offsets::new();
            if present {
                doc_offsets.reserve(tf);
                for _ in 0..tf {
                    let start_at = read_var_u32(offsets, &mut offset_at)?;
                    let len = read_var_u32(offsets, &mut offset_at)?;
                    doc_offsets.push((start_at, start_at + len));
                }
            }

            out.push(Posting::with_weight_and_offsets(doc, doc_positions, doc_offsets, weight));
        }
    }

    Ok(())
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn to_usize(value: u64) -> io::Result<usize> {
    usize::try_from(value).map_err(|_overflow| invalid("offset does not fit in usize"))
}

fn section<'a>(bytes: &'a [u8], offset: u64, len: u64, what: &str) -> io::Result<&'a [u8]> {
    let bad = ||
        io::Error::new(io::ErrorKind::InvalidData, format!("{what} outside segment bounds"));
    let start = to_usize(offset)?;
    let end = start.checked_add(to_usize(len)?).ok_or_else(bad)?;
    if end > bytes.len().saturating_sub(FOOTER_LEN) {
        return Err(bad());
    }
    bytes.get(start..end).ok_or_else(bad)
}
// We need to search this segment for sure
impl SearchIndex for DiskSegment {
    #[timed(search)]
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        match self.dictionary.get(xpath, term) {
            Some(meta) => self.read_postings(meta),
            None => PostingList::default(),
        }
    }
    #[timed(search)]
    fn lookup_term(&self, term: &str, xpath: XPathId) -> TermPostings {
        let Some(meta) = self.dictionary.get(xpath, term) else {
            return TermPostings {
                postings: PostingList::default(),
                doc_freq: 0,
                max_weight: 0,
            };
        };

        TermPostings {
            postings: self.read_postings(meta),
            doc_freq: meta.doc_freq,
            max_weight: meta.max_weight,
        }
    }

    fn resolve_array_rows(&self, rows: &RoaringTreemap) -> RoaringTreemap {
        rows.iter()
            .filter_map(|raw| self.array_row_index.doc_of(ArrayRowId::from_raw(raw)))
            .collect()
    }

    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId> {
        self.array_row_index.parent_of(row)
    }

    fn element_index_of_row(&self, row: ArrayRowId) -> Option<u32> {
        self.array_row_index.element_index_of(row)
    }

    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId> {
        self.array_row_index.doc_of(row)
    }

    fn bool_true_ids(&self, xpath: XPathId) -> RoaringTreemap {
        self.bool_fields
            .get(&xpath)
            .map(|f| f.true_ids.clone())
            .unwrap_or_default()
    }
    fn bool_false_ids(&self, xpath: XPathId) -> RoaringTreemap {
        self.bool_fields
            .get(&xpath)
            .map(|f| f.false_ids.clone())
            .unwrap_or_default()
    }

    fn bool_value(&self, xpath: XPathId, doc_id: DocId) -> Option<bool> {
        let f = self.bool_fields.get(&xpath)?;
        if f.true_ids.contains(doc_id) {
            Some(true)
        } else if f.false_ids.contains(doc_id) {
            Some(false)
        } else {
            None
        }
    }

    #[timed(search)]
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        self.dictionary.get(xpath, term).map_or(0, |meta| meta.doc_freq)
    }
    fn terms(&self, xpath: XPathId) -> Vec<String> {
        self.dictionary
            .field(xpath)
            .map(|dict| {
                dict.entries()
                    .into_iter()
                    .map(|(term, _)| term)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }
    fn term_cursor(&self, term: &str, xpath: XPathId) -> Option<TermCursorInfo<'_>> {
        let meta = self.dictionary.get(xpath, term)?;
        let cursor = BlockCursor::new(self.term_bytes(meta)?, meta).ok()?;
        Some(TermCursorInfo {
            cursor: TermCursor::Block(Box::new(cursor)),
            doc_freq: meta.doc_freq,
            max_weight: meta.max_weight,
        })
    }
    #[timed(search)]
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let Some(dict) = self.dictionary.field(xpath) else {
            return PostingList::default();
        };

        // choose the terms from the dictionary first, then read postings only for those
        let mut matches = dict.prefix(prefix);
        matches.sort_unstable_by(|a, b|
            b.1.doc_freq.cmp(&a.1.doc_freq).then_with(|| a.0.cmp(&b.0))
        );
        matches.truncate(MAX_PATTERN_TERMS);

        let mut postings = Vec::new();
        for (_, meta) in matches {
            self.read_postings_into(meta, &mut postings);
        }

        PostingList::from_items(postings)
    }

    #[timed(search)]
    fn lookup_wildcard(
        &self,
        pattern: &crate::wildcard::WildcardPattern,
        xpath: XPathId
    ) -> PostingList {
        if pattern.is_prefix_only() {
            return self.lookup_prefix(pattern.prefix(), xpath);
        }
        let Some(dict) = self.dictionary.field(xpath) else {
            return PostingList::default();
        };

        // prefix("") yields every term in the field, which is what a leading "*" wildcard needs
        let mut matches: Vec<_> = dict
            .prefix(pattern.prefix())
            .into_iter()
            .filter(|(term, _)| pattern.matches(term))
            .collect();
        matches.sort_unstable_by(|a, b|
            b.1.doc_freq.cmp(&a.1.doc_freq).then_with(|| a.0.cmp(&b.0))
        );
        matches.truncate(MAX_PATTERN_TERMS);

        let mut postings = Vec::new();
        for (_, meta) in matches {
            self.read_postings_into(meta, &mut postings);
        }

        PostingList::from_items(postings)
    }

    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let Some(dict) = self.dictionary.field(xpath) else {
            return PostingList::default();
        };

        let mut postings = Vec::new();

        for (_, meta) in dict.fuzzy(term, opts) {
            self.read_postings_into(meta, &mut postings);
        }

        PostingList::from_items(postings)
    }

    fn fuzzy_expansions(
        &self,
        term: &str,
        xpath: XPathId,
        opts: FuzzyOptions
    ) -> Vec<FuzzyExpansion> {
        self.dictionary
            .field(xpath)
            .map(|dict| {
                dict.fuzzy_expansions(term, opts)
                    .into_iter()
                    .map(|(expansion, _)| expansion)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn prefix_terms(&self, prefix: &str, xpath: XPathId) -> Vec<(String, u32)> {
        self.dictionary
            .field(xpath)
            .map(|dict| {
                dict.prefix(prefix)
                    .into_iter()
                    .map(|(t, meta)| (t, meta.doc_freq))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn validate_footer(bytes: &[u8], footer: &SegmentFooter) -> io::Result<()> {
    if to_usize(footer.dictionary_offset)? < HEADER_LEN {
        return Err(invalid("dictionary starts before segment body"));
    }
    if to_usize(footer.numeric_fields_offset)? < HEADER_LEN {
        return Err(invalid("numeric fields start before segment body"));
    }
    section(bytes, footer.dictionary_offset, footer.dictionary_len, "dictionary")?;
    section(bytes, footer.numeric_fields_offset, footer.numeric_fields_len, "numeric fields")?;
    section(bytes, footer.bool_fields_offset, footer.bool_fields_len, "bool fields")?;
    section(bytes, footer.array_row_index_offset, footer.array_row_index_len, "array row index")?;
    if footer.term_count == 0 && footer.dictionary_len != 4 {
        return Err(invalid("empty dictionary has invalid length"));
    }
    Ok(())
}

fn validate_header(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() < 12 + FOOTER_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "segment too small"));
    }

    if bytes.first_chunk::<8>() != Some(&MAGIC) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad segment magic"));
    }

    let version = read_u32_at(bytes, 8)?;
    if version != VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported segment version"));
    }

    Ok(())
}

fn read_footer(bytes: &[u8]) -> io::Result<SegmentFooter> {
    let start = bytes.len() - FOOTER_LEN;

    Ok(SegmentFooter {
        doc_lengths_offset: read_u64_at(bytes, start)?,
        doc_lengths_len: read_u64_at(bytes, start + 8)?,
        dictionary_offset: read_u64_at(bytes, start + 16)?,
        dictionary_len: read_u64_at(bytes, start + 24)?,
        numeric_fields_offset: read_u64_at(bytes, start + 32)?,
        numeric_fields_len: read_u64_at(bytes, start + 40)?,
        term_count: read_u32_at(bytes, start + 48)?,
        min_doc_id: read_u64_at(bytes, start + 52)?,
        max_doc_id: read_u64_at(bytes, start + 60)?,
        bool_fields_offset: read_u64_at(bytes, start + 84)?,
        bool_fields_len: read_u64_at(bytes, start + 92)?,
        array_row_index_offset: read_u64_at(bytes, start + 68)?,
        array_row_index_len: read_u64_at(bytes, start + 76)?,
    })
}

/// The field lengths of a segment's documents. Scoring asks for one per scored posting, so a field
/// most of the segment's documents have is a dense array over the segment's doc ids (a lookup is
/// an index instead of a tree walk, and 4 bytes per document instead of a tree entry); a field
/// few documents have keeps a map.
pub struct DocLengths {
    fields: Vec<(XPathId, FieldLengths)>,
}

enum FieldLengths {
    // `u32::MAX`: the document has no such field
    Dense {
        first: DocId,
        lens: Vec<u32>,
    },
    Sparse(BTreeMap<DocId, u32>),
}

impl DocLengths {
    fn from_entries(entries: Vec<(DocId, XPathId, u32)>) -> Self {
        let mut by_field: BTreeMap<XPathId, Vec<(DocId, u32)>> = BTreeMap::new();
        for (doc_id, xpath, len) in entries {
            by_field.entry(xpath).or_default().push((doc_id, len));
        }
        let fields = by_field
            .into_iter()
            .map(|(xpath, docs)| {
                let first = docs
                    .iter()
                    .map(|&(doc, _)| doc)
                    .min()
                    .unwrap_or(0);
                let last = docs
                    .iter()
                    .map(|&(doc, _)| doc)
                    .max()
                    .unwrap_or(0);
                let span = last.saturating_sub(first).saturating_add(1);
                // dense when at least a quarter of the span has the field
                let lengths = match usize::try_from(span) {
                    Ok(span_len) if span <= (docs.len() as u64).saturating_mul(4) => {
                        let mut lens = vec![u32::MAX; span_len];
                        for (doc, len) in docs {
                            let slot = usize
                                ::try_from(doc - first)
                                .ok()
                                .and_then(|index| lens.get_mut(index));
                            if let Some(slot) = slot {
                                *slot = len;
                            }
                        }
                        FieldLengths::Dense { first, lens }
                    }
                    _ => FieldLengths::Sparse(docs.into_iter().collect()),
                };
                (xpath, lengths)
            })
            .collect();
        Self { fields }
    }

    pub fn get(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        let (_, lengths) = self.fields.iter().find(|(field, _)| *field == xpath)?;
        match lengths {
            FieldLengths::Dense { first, lens } => {
                let index = usize::try_from(doc_id.checked_sub(*first)?).ok()?;
                lens.get(index)
                    .copied()
                    .filter(|&len| len != u32::MAX)
            }
            FieldLengths::Sparse(map) => map.get(&doc_id).copied(),
        }
    }

    /// Every (document, field) length, field by field.
    pub fn iter(&self) -> impl Iterator<Item = ((DocId, XPathId), u32)> + '_ {
        self.fields.iter().flat_map(
            |(xpath, lengths)| -> Box<dyn Iterator<Item = ((DocId, XPathId), u32)> + '_> {
                let xpath = *xpath;
                match lengths {
                    FieldLengths::Dense { first, lens } =>
                        Box::new(
                            lens
                                .iter()
                                .enumerate()
                                .filter(|&(_, &len)| len != u32::MAX)
                                .map(move |(index, &len)| ((*first + (index as DocId), xpath), len))
                        ),
                    FieldLengths::Sparse(map) => {
                        Box::new(map.iter().map(move |(&doc, &len)| ((doc, xpath), len)))
                    }
                }
            }
        )
    }
}

fn read_doc_lengths(bytes: &[u8], footer: &SegmentFooter) -> io::Result<DocLengths> {
    let data = section(bytes, footer.doc_lengths_offset, footer.doc_lengths_len, "doc lengths")?;
    let mut cursor = Cursor::new(data);
    let count = cursor.read_u32()? as usize;
    // each entry is 16 bytes: a bogus count in a corrupt file must not allocate gigabytes
    let mut entries = Vec::with_capacity(count.min(data.len() / 16));

    for _ in 0..count {
        let doc_id = cursor.read_u64()?; // read doc_id
        let xpath = cursor.read_u32()?;
        let len = cursor.read_u32()?;
        entries.push((doc_id, xpath, len));
    }

    Ok(DocLengths::from_entries(entries))
}

fn read_array_row_index(bytes: &[u8], footer: &SegmentFooter) -> io::Result<ArrayRowIndex> {
    let data = section(bytes, footer.array_row_index_offset, footer.array_row_index_len, "array row index")?;
    let mut cursor = Cursor::new(data);
    let base = ArrayRowId::from_raw(cursor.read_u64()?);
    let count = cursor.read_u32()? as usize;
    let mut idx = ArrayRowIndex::new(base);
    for i in 0..count {
        let doc = cursor.read_u64()?;
        let parent = cursor.read_u64()?;
        let element = cursor.read_u32()?;
        idx.push_row(
            ArrayRowId::from_raw(base.raw() + (i as u64)),
            doc,
            (parent != NO_PARENT_ROW.raw()).then_some(ArrayRowId::from_raw(parent)),
            element
        );
    }
    Ok(idx)
}

// One field is [xpath][term_count][fst_len][fst bytes][TermMeta * term_count].
// The FST maps term bytes to an ord, and that ord indexes the metas array.
// One field is [xpath][term_count][fst_len][fst bytes][TermMeta * term_count].
// The FST maps term bytes to an ord, and that ord indexes the metas array.
fn read_term_dictionary(bytes: &[u8], footer: &SegmentFooter) -> io::Result<TermDictionary> {
   let data = section(bytes, footer.dictionary_offset, footer.dictionary_len, "dictionary")?;
    let mut cursor = Cursor::new(data);
    let field_count = cursor.read_u32()? as usize;

    // Every field costs at least xpath + term_count + fst_len.
    if field_count.saturating_mul(16) > data.len() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidData,
                "dictionary field count exceeds section size"
            )
        );
    }

    let mut dictionary = TermDictionary::new();
    let mut term_count = 0usize;

    for _ in 0..field_count {
        let xpath = cursor.read_u32()?;
        let field_terms = cursor.read_u32()? as usize;
        let fst_len = to_usize(cursor.read_u64()?)?;

        let fst_bytes = cursor.take(fst_len)?.to_vec();

        // Check the metas actually fit before reserving, so a bogus count in a
        // corrupt file cannot make us allocate gigabytes.
        if field_terms.saturating_mul(TERM_META_LEN) > cursor.remaining() {
            return Err(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "dictionary term count exceeds section size"
                )
            );
        }

        let mut metas = Vec::with_capacity(field_terms);

        for _ in 0..field_terms {
            metas.push(TermMeta {
                postings_offset: cursor.read_u64()?,
                postings_len: cursor.read_u32()?,
                doc_freq: cursor.read_u32()?,
                max_weight: cursor.read_u16()?,
                block_count: cursor.read_u32()?,
                block_len: cursor.read_u32()?,
                positions_len: cursor.read_u32()?,
            });
        }

        dictionary.insert_field(xpath, TermDict::from_parts(fst_bytes, metas)?);
        term_count += field_terms;
    }

    if term_count != (footer.term_count as usize) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "dictionary term count mismatch"));
    }

    Ok(dictionary)
}

fn read_numeric_fields(bytes: &[u8], footer: &SegmentFooter) -> io::Result<NumericFields> {
    let data = section(bytes, footer.numeric_fields_offset, footer.numeric_fields_len, "numeric fields")?;
    let mut cursor = Cursor::new(data);
    let xpath_count = cursor.read_u32()? as usize;
    let mut fields = NumericFields::default();

    for _ in 0..xpath_count {
        let xpath = cursor.read_u32()?;
        let kind = NumericKind::from_byte(cursor.read_u8()?).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unknown numeric field kind")
        })?;

        // BKD: packed value order.
        let point_count = cursor.read_u32()? as usize;
        let mut bkd_points = Vec::with_capacity(point_count);
        for _ in 0..point_count {
            let packed = cursor.read_u64()?;
            let doc_id = cursor.read_u64()?;
            bkd_points.push((packed, doc_id));
        }

        // DocValues: doc_id order.
        let entry_count = cursor.read_u32()? as usize;
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let doc_id = cursor.read_u64()?;
            let packed = cursor.read_u64()?;
            entries.push((doc_id, packed));
        }

        let bkd = Bkd::from_packed(kind, bkd_points).map_err(|e|
            io::Error::new(io::ErrorKind::InvalidData, e)
        )?;
        let doc_values = DocValues::from_packed(kind, entries).map_err(|e|
            io::Error::new(io::ErrorKind::InvalidData, e)
        )?;
        fields.insert_field(xpath, NumericField { bkd, doc_values });
    }

    Ok(fields)
}

fn read_bool_points(
    bytes: &[u8],
    footer: &SegmentFooter
) -> io::Result<BTreeMap<XPathId, BoolField>> {
    let data = section(bytes, footer.bool_fields_offset, footer.bool_fields_len, "bool fields")?;
    let mut cursor = Cursor::new(data);
    let count = cursor.read_u32()? as usize;
    let mut fields = BTreeMap::new();
    for _ in 0..count {
        let xpath = cursor.read_u32()?;
        let true_len = to_usize(cursor.read_u64()?)?;
        let true_ids = RoaringTreemap::deserialize_from(cursor.take(true_len)?)?;
        let false_len = to_usize(cursor.read_u64()?)?;
        let false_ids = RoaringTreemap::deserialize_from(cursor.take(false_len)?)?;
        fields.insert(xpath, BoolField {
            true_ids,
            false_ids,
        });
    }
    Ok(fields)
}

#[timed(search)]
fn read_posting_list(bytes: &[u8], meta: TermMeta) -> io::Result<PostingList> {
    let mut postings = Vec::new();

    read_posting_list_into(bytes, meta, &mut postings)?;

    Ok(PostingList::from_sorted_with_max_weight(postings, meta.max_weight))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> io::Result<u32> {
    array_at(bytes, offset)
        .map(u32::from_le_bytes)
        .ok_or_else(|| invalid("read past end of segment"))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> io::Result<u64> {
    array_at(bytes, offset)
        .map(u64::from_le_bytes)
        .ok_or_else(|| invalid("read past end of segment"))
}

// the most basic cursor for reading bytes, kind of hate it to be honest
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    // fn read_u16(&mut self) -> io::Result<u16> {
    //     let value = u16::from_le_bytes(self.take(2)?.try_into().unwrap());
    //     Ok(value)
    // }

   fn read_array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
    let array = array_at(self.bytes, self.offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected eof"))?;
    self.offset += N;
    Ok(array)
}

fn read_u8(&mut self) -> io::Result<u8> {
    self.read_array().map(u8::from_le_bytes)
}

fn read_u16(&mut self) -> io::Result<u16> {
    self.read_array().map(u16::from_le_bytes)
}

fn read_u32(&mut self) -> io::Result<u32> {
    self.read_array().map(u32::from_le_bytes)
}

fn read_u64(&mut self) -> io::Result<u64> {
    self.read_array().map(u64::from_le_bytes)
}

fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
    let eof = || io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected eof");
    let end = self.offset.checked_add(len).ok_or_else(eof)?;
    let slice = self.bytes.get(self.offset..end).ok_or_else(eof)?;
    self.offset = end;
    Ok(slice)
}

    const fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
}
