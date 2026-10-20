//! Vector file: the int8 vectors of one vector field for the documents of one segment.
//!
//! Layout (little-endian):
//! +------------------+
//! | header           |  magic[8] "CLVECTOR", u32 version, u32 dims, f32 scale,
//! |                  |  u16 model_id_len, model_id bytes, u64 count
//! +------------------+
//! | records          |  (u64 doc_id, i8 row[dims]) * count
//! |                  |  doc_id strictly increasing, so lookups are a binary search
//! +------------------+
//!
//! Stored value = round(component * scale) of an L2-normalized vector, so
//! cosine(query, row) = dot(query, row) / scale for a normalized f32 query.
//! Files are written to `<path>.tmp`, fsynced and renamed, and never modified afterwards.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use memmap2::Mmap;

use super::vector_search::{TopK, VectorHit};
use crate::posting::DeleteSet;
use crate::types::{DocId, XPathId};

pub const MAGIC: [u8; 8] = *b"CLVECTOR";
pub const VERSION: u32 = 1;

const MAX_MODEL_ID_LEN: usize = u8::MAX as usize;
/// magic + version + dims + scale + model_id_len
const FIXED_HEADER_LEN: usize = 8 + 4 + 4 + 4 + 2;

/// `segment-12.idx` + xpath 3 -> `segment-12.3.vec`, in the same directory.
pub fn vector_file_path(segment_path: &Path, xpath: XPathId) -> PathBuf {
    segment_path.with_extension(format!("{xpath}.vec"))
}
/// The vectors of one vector field for one live segment.
#[derive(Debug, Clone)]
pub struct SegmentVectors {
    pub segment: PathBuf,
    pub xpath: XPathId,
    pub file: Arc<VectorFile>,
}
/// Vector files belonging to `segment_path`, as (xpath, path), sorted by xpath.
/// Removes leftover `.vec.tmp` files, so only call it while no embedding is running (at load).
pub fn find_vector_files(segment_path: &Path) -> io::Result<Vec<(XPathId, PathBuf)>> {
    let (Some(dir), Some(stem)) = (
        segment_path.parent(),
        segment_path.file_stem().and_then(|s| s.to_str()),
    ) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{stem}.");
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(&prefix)) else {
            continue;
        };
        if rest.ends_with(".vec.tmp") {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        if let Some(xpath) = rest.strip_suffix(".vec").and_then(|x| x.parse::<XPathId>().ok()) {
            found.push((xpath, entry.path()));
        }
    }
    found.sort_by_key(|(xpath, _)| *xpath);
    Ok(found)
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn invalid_input(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn corrupt(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("vector file {}: {msg}", path.display()),
    )
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("length checked by caller"))
}

fn read_u64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("length checked by caller"))
}

/// Streams records into a new vector file. Nothing is visible at `path` until `finish`.
pub struct VectorFileWriter {
    out: BufWriter<File>,
    tmp_path: PathBuf,
    final_path: PathBuf,
    dims: usize,
    count: u64,
    count_offset: u64,
    last_doc: Option<DocId>,
    scratch: Vec<u8>,
}

impl VectorFileWriter {
    pub fn create(path: impl AsRef<Path>, model_id: &str, dims: usize, scale: f32) -> io::Result<Self> {
        if dims == 0 || dims > u32::MAX as usize {
            return Err(invalid_input(format!("invalid vector dimension {dims}")));
        }
        if model_id.is_empty() || model_id.len() > MAX_MODEL_ID_LEN {
            return Err(invalid_input(format!(
                "model id must be 1..={MAX_MODEL_ID_LEN} bytes, got {}",
                model_id.len()
            )));
        }
        if !(scale.is_finite() && scale > 0.0) {
            return Err(invalid_input(format!("invalid quantization scale {scale}")));
        }

        let final_path = path.as_ref().to_path_buf();
        let tmp_path = tmp_path_for(&final_path);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        let mut out = BufWriter::new(file);
        out.write_all(&MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&(dims as u32).to_le_bytes())?;
        out.write_all(&scale.to_le_bytes())?;
        out.write_all(&(model_id.len() as u16).to_le_bytes())?;
        out.write_all(model_id.as_bytes())?;
        let count_offset = (FIXED_HEADER_LEN + model_id.len()) as u64;
        out.write_all(&0u64.to_le_bytes())?; // patched in finish()

        Ok(Self {
            out,
            tmp_path,
            final_path,
            dims,
            count: 0,
            count_offset,
            last_doc: None,
            scratch: Vec::with_capacity(dims),
        })
    }

    /// Appends one vector. Doc ids must be strictly increasing.
    pub fn push(&mut self, doc_id: DocId, row: &[i8]) -> io::Result<()> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.extend(row.iter().map(|&v| v as u8));
        let result = self.push_bytes(doc_id, &scratch);
        self.scratch = scratch;
        result
    }

    fn push_bytes(&mut self, doc_id: DocId, row: &[u8]) -> io::Result<()> {
        if row.len() != self.dims {
            return Err(invalid_input(format!(
                "row for doc {doc_id} has {} dims, expected {}",
                row.len(),
                self.dims
            )));
        }
        if let Some(last) = self.last_doc {
            if doc_id <= last {
                return Err(invalid_input(format!(
                    "doc ids must be strictly increasing: {doc_id} after {last}"
                )));
            }
        }
        self.out.write_all(&doc_id.to_le_bytes())?;
        self.out.write_all(row)?;
        self.last_doc = Some(doc_id);
        self.count += 1;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.count
    }

    /// Writes the record count, fsyncs and atomically moves the file into place.
    pub fn finish(self) -> io::Result<()> {
        let mut file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(self.count_offset))?;
        file.write_all(&self.count.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.tmp_path, &self.final_path)?;
        if let Some(dir) = self.final_path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

/// Read-only, memory-mapped vector file.
#[derive(Debug)]
pub struct VectorFile {
    mmap: Mmap,
    path: PathBuf,
    model_id: String,
    dims: usize,
    scale: f32,
    count: usize,
    records_offset: usize,
    record_len: usize,
}

impl VectorFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        // Safe in practice: vector files are never modified after the rename in finish().
        let mmap = unsafe { Mmap::map(&file)? };
        let data = &mmap[..];

        if data.len() < FIXED_HEADER_LEN {
            return Err(corrupt(&path, "shorter than its header"));
        }
        if data[0..8] != MAGIC {
            return Err(corrupt(&path, "bad magic"));
        }
        let version = read_u32(data, 8);
        if version != VERSION {
            return Err(corrupt(&path, format!("unsupported version {version}")));
        }
        let dims = read_u32(data, 12) as usize;
        if dims == 0 {
            return Err(corrupt(&path, "zero dimensions"));
        }
        let scale = f32::from_le_bytes(data[16..20].try_into().expect("length checked above"));
        if !(scale.is_finite() && scale > 0.0) {
            return Err(corrupt(&path, format!("invalid scale {scale}")));
        }
        let id_len = u16::from_le_bytes(data[20..22].try_into().expect("length checked above")) as usize;
        let id_end = FIXED_HEADER_LEN + id_len;
        if data.len() < id_end + 8 {
            return Err(corrupt(&path, "truncated header"));
        }
        let model_id = std::str::from_utf8(&data[FIXED_HEADER_LEN..id_end])
            .map_err(|e| corrupt(&path, format!("model id is not UTF-8: {e}")))?
            .to_string();
        let count = usize::try_from(read_u64(data, id_end))
            .map_err(|_| corrupt(&path, "record count does not fit in memory"))?;
        let records_offset = id_end + 8;
        let record_len = 8 + dims;
        let expected_len = count
            .checked_mul(record_len)
            .and_then(|n| n.checked_add(records_offset));
        if expected_len != Some(data.len()) {
            return Err(corrupt(
                &path,
                format!("length {} does not match {count} records of {dims} dims", data.len()),
            ));
        }

        Ok(Self {
            mmap,
            path,
            model_id,
            dims,
            scale,
            count,
            records_offset,
            record_len,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// False when the file was written by a different model and must be re-embedded.
    pub fn matches(&self, model_id: &str, dims: usize) -> bool {
        self.model_id == model_id && self.dims == dims
    }

    fn record_start(&self, index: usize) -> usize {
        self.records_offset + index * self.record_len
    }

    pub fn doc_id(&self, index: usize) -> DocId {
        read_u64(&self.mmap, self.record_start(index))
    }

    fn row_bytes(&self, index: usize) -> &[u8] {
        let start = self.record_start(index) + 8;
        &self.mmap[start..start + self.dims]
    }

    pub fn row(&self, index: usize) -> Vec<i8> {
        self.row_bytes(index).iter().map(|&b| b as i8).collect()
    }

    pub fn contains(&self, doc_id: DocId) -> bool {
        let (mut lo, mut hi) = (0usize, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.doc_id(mid).cmp(&doc_id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    fn score(&self, index: usize, query: &[f32]) -> f32 {
        let dot: f32 = self
            .row_bytes(index)
            .iter()
            .zip(query)
            .map(|(&b, &q)| (b as i8) as f32 * q)
            .sum();
        dot / self.scale
    }

    /// Exact top-k by cosine similarity, skipping deleted documents.
    /// `query` must be L2-normalized and have `dims` components.
    pub fn search(&self, query: &[f32], k: usize, deleted: &DeleteSet) -> io::Result<Vec<VectorHit>> {
        if query.len() != self.dims {
            return Err(invalid_input(format!(
                "query has {} dims, file {} has {}",
                query.len(),
                self.path.display(),
                self.dims
            )));
        }
        let mut top = TopK::new(k);
        for index in 0..self.count {
            let doc_id = self.doc_id(index);
            if deleted.contains(doc_id) {
                continue;
            }
            top.offer(doc_id, self.score(index, query));
        }
        Ok(top.into_sorted())
    }
}
/// Merges vector files into one, dropping deleted documents. Used by compaction.
/// If the same document appears in several inputs, the row from the last input wins.
/// Returns the number of records written. On failure no file is left at `output`.

pub fn merge_vector_files(
    inputs: &[&VectorFile],
    deleted: &DeleteSet,
    output: impl AsRef<Path>,
) -> io::Result<u64> {
    let output = output.as_ref();
    let result = merge_into(inputs, deleted, output);
    if result.is_err() {
        let _ = fs::remove_file(tmp_path_for(output));
    }
    result
}
/// Merges vector files into one, dropping deleted documents. Used by compaction.
/// If the same document appears in several inputs, the row from the last input wins.
/// Returns the number of records written.
fn merge_into(
    inputs: &[&VectorFile],
    deleted: &DeleteSet,
    output: impl AsRef<Path>,
) -> io::Result<u64> {
    let first = inputs
        .first()
        .ok_or_else(|| invalid_input("merge_vector_files needs at least one input"))?;
    for file in inputs {
        if file.model_id != first.model_id || file.dims != first.dims || file.scale != first.scale {
            return Err(invalid_input(format!(
                "cannot merge {} ({}, {} dims) with {} ({}, {} dims)",
                file.path.display(),
                file.model_id,
                file.dims,
                first.path.display(),
                first.model_id,
                first.dims
            )));
        }
    }

    let mut writer = VectorFileWriter::create(output, &first.model_id, first.dims, first.scale)?;
    let mut cursors = vec![0usize; inputs.len()];

    loop {
        let mut next: Option<DocId> = None;
        for (n, file) in inputs.iter().enumerate() {
            if cursors[n] < file.len() {
                let doc = file.doc_id(cursors[n]);
                if next.is_none_or(|best| doc < best) {
                    next = Some(doc);
                }
            }
        }
        let Some(doc) = next else { break };

        let mut source = None;
        for (n, file) in inputs.iter().enumerate() {
            if cursors[n] < file.len() && file.doc_id(cursors[n]) == doc {
                source = Some((n, cursors[n]));
                cursors[n] += 1;
            }
        }
        let (n, index) = source.expect("at least one input holds the selected doc id");
        if !deleted.contains(doc) {
            writer.push_bytes(doc, inputs[n].row_bytes(index))?;
        }
    }

    let written = writer.len();
    writer.finish()?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corelamo-vector-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, rows: &[(DocId, [i8; 3])]) {
        let mut w = VectorFileWriter::create(path, "test-model", 3, 127.0).unwrap();
        for (doc, row) in rows {
            w.push(*doc, row).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn round_trip_and_search() {
        let path = temp_dir("roundtrip").join("segment-1.4.vec");
        write(&path, &[(10, [127, 0, 0]), (20, [0, 127, 0]), (30, [90, 90, 0])]);

        let file = VectorFile::open(&path).unwrap();
        assert_eq!((file.model_id(), file.dims(), file.len()), ("test-model", 3, 3));
        assert!(file.matches("test-model", 3));
        assert!(!file.matches("other-model", 3));
        assert!(file.contains(20) && !file.contains(25));
        assert_eq!(file.row(2), vec![90, 90, 0]);

        let hits = file.search(&[1.0, 0.0, 0.0], 2, &DeleteSet::new()).unwrap();
        let docs: Vec<DocId> = hits.iter().map(|h| h.doc_id).collect();
        assert_eq!(docs, vec![10, 30]);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
        assert!(!path.with_extension("4.vec.tmp").exists());
    }

    #[test]
    fn search_skips_deleted() {
        let path = temp_dir("deleted").join("v.vec");
        write(&path, &[(1, [127, 0, 0]), (2, [100, 10, 0])]);
        let mut deleted = DeleteSet::new();
        deleted.delete(1);
        let hits = VectorFile::open(&path).unwrap().search(&[1.0, 0.0, 0.0], 5, &deleted).unwrap();
        assert_eq!(hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn rejects_unsorted_and_wrong_width() {
        let path = temp_dir("unsorted").join("v.vec");
        let mut w = VectorFileWriter::create(&path, "m", 3, 127.0).unwrap();
        w.push(5, &[1, 2, 3]).unwrap();
        assert_eq!(w.push(5, &[1, 2, 3]).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(w.push(6, &[1, 2]).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_truncated_file() {
        let path = temp_dir("truncated").join("v.vec");
        write(&path, &[(1, [1, 2, 3]), (2, [4, 5, 6])]);
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 1).unwrap();
        assert_eq!(VectorFile::open(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn query_dimension_mismatch_is_an_error() {
        let path = temp_dir("dims").join("v.vec");
        write(&path, &[(1, [1, 2, 3])]);
        let file = VectorFile::open(&path).unwrap();
        assert!(file.search(&[1.0, 0.0], 1, &DeleteSet::new()).is_err());
    }

    #[test]
    fn merge_interleaves_and_drops_deleted() {
        let dir = temp_dir("merge");
        let (a, b, out) = (dir.join("a.vec"), dir.join("b.vec"), dir.join("out.vec"));
        write(&a, &[(1, [1, 0, 0]), (4, [4, 0, 0])]);
        write(&b, &[(2, [2, 0, 0]), (3, [3, 0, 0]), (4, [9, 0, 0])]);
        let (fa, fb) = (VectorFile::open(&a).unwrap(), VectorFile::open(&b).unwrap());
        let mut deleted = DeleteSet::new();
        deleted.delete(3);

        assert_eq!(merge_vector_files(&[&fa, &fb], &deleted, &out).unwrap(), 3);
        let merged = VectorFile::open(&out).unwrap();
        let docs: Vec<DocId> = (0..merged.len()).map(|i| merged.doc_id(i)).collect();
        assert_eq!(docs, vec![1, 2, 4]);
        assert_eq!(merged.row(2), vec![9, 0, 0], "later input wins for a duplicated doc");
    }

    #[test]
    fn merge_refuses_mixed_models() {
        let dir = temp_dir("mixed");
        let (a, b) = (dir.join("a.vec"), dir.join("b.vec"));
        write(&a, &[(1, [1, 0, 0])]);
        let mut w = VectorFileWriter::create(&b, "other-model", 3, 127.0).unwrap();
        w.push(2, &[1, 0, 0]).unwrap();
        w.finish().unwrap();
        let (fa, fb) = (VectorFile::open(&a).unwrap(), VectorFile::open(&b).unwrap());
        assert!(merge_vector_files(&[&fa, &fb], &DeleteSet::new(), dir.join("o.vec")).is_err());
    }

    #[test]
    fn path_sits_next_to_segment() {
        assert_eq!(
            vector_file_path(Path::new("/db/index/segment-12.idx"), 3),
            PathBuf::from("/db/index/segment-12.3.vec")
        );
    }
    #[test]
    fn finds_only_this_segments_files_and_removes_tmp() {
        let dir = temp_dir("find");
        let segment = dir.join("segment-12.idx");
        fs::write(&segment, b"").unwrap();
        write(&dir.join("segment-12.3.vec"), &[(1, [1, 0, 0])]);
        write(&dir.join("segment-120.3.vec"), &[(1, [1, 0, 0])]);
        fs::write(dir.join("segment-12.5.vec.tmp"), b"partial").unwrap();

        let found = find_vector_files(&segment).unwrap();
        assert_eq!(found, vec![(3, dir.join("segment-12.3.vec"))]);
        assert!(!dir.join("segment-12.5.vec.tmp").exists());
    }
}