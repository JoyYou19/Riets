use std::{io, path::Path};

use memmap2::Mmap;

use crate::{
    disk::{
        codec::{read_var_u16, read_var_u32, read_var_u64},
        format::{SegmentFooter, TermEntry, FOOTER_LEN, MAGIC, VERSION},
    },
    posting::{Posting, PostingList},
    search::SearchIndex,
};
/*
* MMaps a disk segment and implements SearchIndex
*/

// Read only disk segment.
// Segmetn file layout is roughly [header][posting bytes][dictionary bytes][footer]
// the whole file is MMAPED we are cool
#[derive(Debug)]
pub struct DiskSegment {
    mmap: Mmap,
    dictionary: Vec<TermEntry>,
}

impl DiskSegment {
    // Opens and validates a segment, doesn't do any decoding for postings,
    // just mmaps, validates, reads the dictionary, so we can use this
    // even if we don't need to use the data inside of it
    // in the case of just checking if we even want to read this segment
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        validate_header(&mmap)?;

        let footer = read_footer(&mmap)?;
        validate_footer(&mmap, &footer)?;

        let dictionary = read_dictionary(&mmap, &footer)?;

        Ok(Self { mmap, dictionary })
    }

    // Decodes the postings, specifically for the purpose of reading what is inside the actual data
    fn read_postings(&self, entry: &TermEntry) -> PostingList {
        let start = entry.postings_offset as usize;
        let len = entry.postings_len as usize;

        let Some(end) = start.checked_add(len) else {
            return PostingList::default();
        };

        if end > self.mmap.len() {
            return PostingList::default();
        }

        read_posting_list(&self.mmap[start..end], entry.doc_freq).unwrap_or_default()
    }

    // Finds the first dictionary position where we simply check if it is bigger than the target
    fn lower_bound_term(&self, term: &str, xpath: crate::types::XPathId) -> usize {
        self.dictionary
            .partition_point(|entry| (entry.xpath, entry.term.as_str()) < (xpath, term))
    }

    // Reads posting the same way as the original function, but into a buffer
    fn read_postings_into(&self, entry: &TermEntry, out: &mut Vec<Posting>) {
        let start = entry.postings_offset as usize;
        let len = entry.postings_len as usize;

        let Some(end) = start.checked_add(len) else {
            return;
        };

        if end > self.mmap.len() {
            return;
        }

        let _ = read_posting_list_into(&self.mmap[start..end], entry.doc_freq, out);
    }

    // Love this function, amazing, beautiful, great, lovely.
    pub fn to_immutable_segment(&self) -> crate::segment::ImmutableSegment {
        let mut terms = std::collections::BTreeMap::new();

        for entry in &self.dictionary {
            let postings = self.read_postings(entry);

            if postings.is_empty() {
                continue;
            }

            terms.insert(
                crate::types::TermKey::new(entry.term.clone(), entry.xpath),
                postings,
            );
        }

        crate::segment::ImmutableSegment::new(terms)
    }
}

fn read_posting_list_into(bytes: &[u8], doc_freq: u32, out: &mut Vec<Posting>) -> io::Result<()> {
    let mut offset = 0;
    let count = doc_freq as usize;

    out.reserve(count);

    let mut last_doc_id = 0u64;

    for _ in 0..count {
        let doc_delta = read_var_u64(bytes, &mut offset)?;
        let doc_id = last_doc_id + doc_delta;
        last_doc_id = doc_id;

        let weight = read_var_u16(bytes, &mut offset)?;
        let position_count = read_var_u32(bytes, &mut offset)? as usize;

        let mut positions = Vec::with_capacity(position_count);
        let mut last_position = 0u32;

        for _ in 0..position_count {
            let position_delta = read_var_u32(bytes, &mut offset)?;
            let position = last_position + position_delta;
            last_position = position;
            positions.push(position);
        }

        out.push(Posting::with_weight(doc_id, positions, weight));
    }

    Ok(())
}

// We need to search this segment for sure
impl SearchIndex for DiskSegment {
    fn lookup(&self, term: &str, xpath: crate::types::XPathId) -> PostingList {
        match self
            .dictionary
            .binary_search_by(|entry| (entry.xpath, entry.term.as_str()).cmp(&(xpath, term)))
        {
            Ok(index) => self.read_postings(&self.dictionary[index]),
            Err(_) => PostingList::default(),
        }
    }

    fn lookup_prefix(&self, prefix: &str, xpath: crate::types::XPathId) -> PostingList {
        let start = self.lower_bound_term(prefix, xpath);
        let mut postings = Vec::new();

        for entry in &self.dictionary[start..] {
            if entry.xpath != xpath || !entry.term.starts_with(prefix) {
                break;
            }

            self.read_postings_into(entry, &mut postings);
        }

        PostingList::from_items(postings)
    }

    fn lookup_wildcard(
        &self,
        pattern: &crate::wildcard::WildcardPattern,
        xpath: crate::types::XPathId,
    ) -> PostingList {
        if pattern.is_prefix_only() {
            return self.lookup_prefix(pattern.prefix(), xpath);
        }

        let prefix = pattern.prefix();
        let start = self.lower_bound_term(prefix, xpath);
        let mut postings = Vec::new();

        for entry in &self.dictionary[start..] {
            if entry.xpath != xpath {
                break;
            }

            if !prefix.is_empty() && !entry.term.starts_with(prefix) {
                break;
            }

            if pattern.matches(&entry.term) {
                self.read_postings_into(entry, &mut postings);
            }
        }

        PostingList::from_items(postings)
    }
}

fn validate_footer(bytes: &[u8], footer: &SegmentFooter) -> io::Result<()> {
    let dictionary_start = footer.dictionary_offset as usize;
    let dictionary_len = footer.dictionary_len as usize;

    let Some(dictionary_end) = dictionary_start.checked_add(dictionary_len) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary offset overflow",
        ));
    };

    if dictionary_start < crate::disk::format::HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary starts before segment body",
        ));
    }

    if dictionary_end > bytes.len() - FOOTER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary outside segment bounds",
        ));
    }

    if footer.term_count == 0 && footer.dictionary_len != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty dictionary has invalid length",
        ));
    }

    Ok(())
}

fn validate_header(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() < 12 + FOOTER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "segment too small",
        ));
    }

    if bytes[0..8] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad segment magic",
        ));
    }

    let version = read_u32_at(bytes, 8);
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported segment version",
        ));
    }

    Ok(())
}

fn read_footer(bytes: &[u8]) -> io::Result<SegmentFooter> {
    let start = bytes.len() - FOOTER_LEN;

    Ok(SegmentFooter {
        dictionary_offset: read_u64_at(bytes, start),
        dictionary_len: read_u64_at(bytes, start + 8),
        term_count: read_u32_at(bytes, start + 16),
    })
}

fn read_dictionary(bytes: &[u8], footer: &SegmentFooter) -> io::Result<Vec<TermEntry>> {
    let start = footer.dictionary_offset as usize;
    let len = footer.dictionary_len as usize;

    let Some(end) = start.checked_add(len) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary offset overflow",
        ));
    };

    if end > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary outside segment bounds",
        ));
    }

    let mut cursor = Cursor::new(&bytes[start..end]);
    let count = cursor.read_u32()? as usize;

    if count != footer.term_count as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary term count mismatch",
        ));
    }

    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        entries.push(TermEntry {
            term: cursor.read_string()?,
            xpath: cursor.read_u32()?,
            postings_offset: cursor.read_u64()?,
            postings_len: cursor.read_u32()?,
            doc_freq: cursor.read_u32()?,
        });
    }

    Ok(entries)
}

fn read_posting_list(bytes: &[u8], doc_freq: u32) -> io::Result<PostingList> {
    let mut postings = Vec::new();
    read_posting_list_into(bytes, doc_freq, &mut postings)?;
    Ok(PostingList::from_items(postings))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

// the most basic cursor for reading bytes, kind of hate it to be honest
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        let value = u16::from_le_bytes(self.take(2)?.try_into().unwrap());
        Ok(value)
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let value = u32::from_le_bytes(self.take(4)?.try_into().unwrap());
        Ok(value)
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        let value = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
        Ok(value)
    }

    // WARN: Keep in mind the utf8 encoding brodie
    fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf8 term"))
    }

    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self.offset + len;

        if end > self.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected eof",
            ));
        }

        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{analyzer::analyzer::Analyzer, disk::writer::write_segment, mem::MemIndex};

    #[test]
    fn can_read_written_segment_from_disk() {
        let analyzer = Analyzer::new();
        let mut index = MemIndex::new();

        index.add_document(&analyzer, 1, 0, "rust database engine");
        index.add_document(&analyzer, 2, 0, "database storage");

        let segment = index.freeze();

        let path = std::env::temp_dir().join(format!(
            "corelamo-test-read-segment-{}.idx",
            std::process::id()
        ));

        write_segment(&path, &segment).unwrap();

        let disk = DiskSegment::open(&path).unwrap();
        let result = disk.lookup("database", 0);

        let ids: Vec<_> = result.items().iter().map(|p| p.doc_id).collect();
        assert_eq!(ids, vec![1, 2]);

        std::fs::remove_file(path).unwrap();
    }

    fn write_test_segment() -> (std::path::PathBuf, MemIndex) {
        let analyzer = Analyzer::new();
        let mut index = MemIndex::new();

        index.add_document_weighted(&analyzer, 1, 0, "rust database engine", 10, 20);
        index.add_document_weighted(&analyzer, 2, 0, "database storage", 10, 20);
        index.add_document_weighted(&analyzer, 3, 0, "dataset database", 10, 20);

        let segment = index.clone().freeze();

        let path = std::env::temp_dir().join(format!(
            "corelamo-test-read-segment-{}-{}.idx",
            std::process::id(),
            uuid_like()
        ));

        write_segment(&path, &segment).unwrap();

        (path, index)
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn disk_lookup_prefix_matches_mem() {
        let (path, mem) = write_test_segment();
        let disk = DiskSegment::open(&path).unwrap();

        assert_eq!(disk.lookup_prefix("data", 0), mem.lookup_prefix("data", 0));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disk_lookup_wildcard_matches_mem() {
        let (path, mem) = write_test_segment();
        let disk = DiskSegment::open(&path).unwrap();

        let pattern = crate::wildcard::WildcardPattern::parse("dat*");

        assert_eq!(
            disk.lookup_wildcard(&pattern, 0),
            mem.lookup_wildcard(&pattern, 0)
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disk_preserves_positions_and_weights() {
        let (path, mem) = write_test_segment();
        let disk = DiskSegment::open(&path).unwrap();

        assert_eq!(
            disk.lookup("database", 0),
            mem.lookup_or_empty("database", 0)
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disk_missing_term_returns_empty() {
        let (path, _) = write_test_segment();
        let disk = DiskSegment::open(&path).unwrap();

        assert!(disk.lookup("missing", 0).is_empty());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disk_lookup_respects_xpath_ordering() {
        let analyzer = Analyzer::new();
        let mut index = MemIndex::new();

        index.add_document(&analyzer, 1, 0, "database");
        index.add_document(&analyzer, 2, 1, "database");

        let segment = index.freeze();

        let path = std::env::temp_dir().join(format!("corelamo-test-xpath-{}.idx", uuid_like()));

        write_segment(&path, &segment).unwrap();

        let disk = DiskSegment::open(&path).unwrap();

        let ids_0: Vec<_> = disk
            .lookup("database", 0)
            .items()
            .iter()
            .map(|p| p.doc_id)
            .collect();
        let ids_1: Vec<_> = disk
            .lookup("database", 1)
            .items()
            .iter()
            .map(|p| p.doc_id)
            .collect();

        assert_eq!(ids_0, vec![1]);
        assert_eq!(ids_1, vec![2]);

        std::fs::remove_file(path).unwrap();
    }
}
