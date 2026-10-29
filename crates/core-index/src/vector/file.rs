//! Vector file: the vectors of one vector field for the documents of one segment.
//!
//! Layout (little-endian), every section starting on a 64-byte boundary:
//! +-------------+
//! | header      |  magic[8] "CLVECTOR", u32 version, u32 reserved, u64 file_id, u32 dims,
//! |             |  u8 original_kind, u8 quant_kind, u16 model_id_len, f32 quant_absmax,
//! |             |  u32 reserved, u64 count, model_id bytes
//! | doc ids     |  u64 * count, strictly increasing (binary search)
//! | int8 codes  |  i8 * dims * count   hot: scans and graph walks
//! | originals   |  f32 * dims * count  cold: rescoring, compaction, graph build
//! +-------------+
//!
//! Vectors must be L2-normalized, so dot product = cosine. Codes are
//! round(x * 127 / quant_absmax), clipped; `file_id` is random per written file and is what a
//! graph binds to. Files are written to `<path>.tmp`, fsynced and renamed, and never modified.
#[cfg(target_endian = "big")]
compile_error!("vector files are mapped as little-endian slices");

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use memmap2::Mmap;

use super::kernel::QuantRange;
use crate::hash::{fmix64, stable_hash64};
use crate::posting::DeleteSet;
use crate::types::{DocId, XPathId};

pub(super) const MAGIC: [u8; 8] = *b"CLVECTOR";
pub(super) const VERSION: u32 = 2;


const ORIGINAL_F32: u8 = 1;
const QUANT_INT8: u8 = 1;
const MAX_MODEL_ID_LEN: usize = u8::MAX as usize;
/// magic + version + reserved + file_id + dims + original_kind + quant_kind + model_id_len +
/// quant_absmax + reserved + count
const HEADER_FIXED_LEN: usize = 8 + 4 + 4 + 8 + 4 + 1 + 1 + 2 + 4 + 4 + 8;
/// Section alignment; also guarantees the u64 and f32 views are aligned.
pub(crate) const SECTION_ALIGNMENT: usize = 64;
/// Components kept for measuring a file's range; bounds writer memory at ~4 MiB.
const SAMPLE_COMPONENTS: usize = 1 << 20;

/// Vector files belonging to `segment_path`, as (xpath, path), sorted by xpath.
/// Removes leftover temporary files of this segment, so only call it while nothing is
/// writing vectors or graphs (at load).
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
        if rest.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        if let Some(xpath) = rest
            .strip_suffix(".vec")
            .and_then(|x| x.parse::<XPathId>().ok())
        {
            found.push((xpath, entry.path()));
        }
    }
    found.sort_by_key(|(xpath, _)| *xpath);
    Ok(found)
}
/// `segment-12.idx` + xpath 3 -> `segment-12.3.vec`, in the same directory.
pub fn vector_file_path(segment_path: &Path, xpath: XPathId) -> PathBuf {
    segment_path.with_extension(format!("{xpath}.vec"))
}
pub(crate) fn tmp_path_for(path: &Path) -> PathBuf {
    with_suffix(path, ".tmp")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

pub(crate) fn invalid_input(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn corrupt(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("vector file {}: {msg}", path.display()),
    )
}
//round a positon up to the next multiple of 64
fn round_up_to_section(value: usize) -> Option<usize> {
    value
        .checked_add(SECTION_ALIGNMENT - 1)
        .map(|v| v / SECTION_ALIGNMENT * SECTION_ALIGNMENT)
}

/// Byte offsets of every section. The one definition shared by writer and reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    ids: usize,
    codes: usize,
    originals: usize,
    total: usize,
}

impl Layout {
    fn new(dims: usize, model_id_len: usize, count: usize) -> Option<Self> {
        let ids = round_up_to_section(HEADER_FIXED_LEN.checked_add(model_id_len)?)?;
        let codes = round_up_to_section(ids.checked_add(count.checked_mul(8)?)?)?;
        let originals = round_up_to_section(codes.checked_add(count.checked_mul(dims)?)?)?;
        let total = originals.checked_add(count.checked_mul(dims)?.checked_mul(4)?)?;
        Some(Self {
            ids,
            codes,
            originals,
            total,
        })
    }
}

/// Random per written file; collisions only need to be unlikely between the files of one
/// segment and field over its lifetime.
fn new_file_id(path: &Path) -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)//maybe needs to be universal
        .map_or(0, |d| d.as_nanos() as u64);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let process = u64::from(std::process::id());
    fmix64(
        nanos
            ^ fmix64(process.rotate_left(32) ^ sequence)
            ^ stable_hash64(path.as_os_str().as_encoded_bytes()),
    )
}
/// Streams rows into a new vector file. Nothing is visible at `path` until `finish`; dropping
/// the writer without finishing removes every temporary file.
pub struct VectorFileWriter {
    final_path: PathBuf,
    ids_path: PathBuf,
    originals_path: PathBuf,
    ids: Option<BufWriter<File>>,
    originals: Option<BufWriter<File>>,
    model_id: String,
    dims: usize,
    count: u64,
    last_doc: Option<DocId>,
    sample: Vec<f32>,
    sample_rows_cap: usize,
    seed: u64,
    committed: bool,
}

impl VectorFileWriter {
    pub fn create(path: impl AsRef<Path>, model_id: &str, dims: usize) -> io::Result<Self> {
        if dims == 0 || dims > u32::MAX as usize {
            return Err(invalid_input(format!("invalid vector dimension {dims}")));
        }
        if model_id.is_empty() || model_id.len() > MAX_MODEL_ID_LEN {
            return Err(invalid_input(format!(
                "model id must be 1..={MAX_MODEL_ID_LEN} bytes, got {}",
                model_id.len()
            )));
        }
        let final_path = path.as_ref().to_path_buf();
        let ids_path = with_suffix(&final_path, ".ids.tmp");
        let originals_path = with_suffix(&final_path, ".orig.tmp");
        let open = |p: &Path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)
                .map(BufWriter::new)
        };
        let ids = open(&ids_path)?;
        let originals = match open(&originals_path) {
            Ok(file) => file,
            Err(e) => {
                let _ = fs::remove_file(&ids_path);
                return Err(e);
            }
        };
        let sample_rows_cap = (SAMPLE_COMPONENTS / dims).max(1);
        Ok(Self {
            seed: new_file_id(&final_path),
            final_path,
            ids_path,
            originals_path,
            ids: Some(ids),
            originals: Some(originals),
            model_id: model_id.to_string(),
            dims,
            count: 0,
            last_doc: None,
            sample: Vec::with_capacity(sample_rows_cap.saturating_mul(dims).min(SAMPLE_COMPONENTS)),
            sample_rows_cap,
            committed: false,
        })
    }
    /// Appends one L2-normalized vector. Doc ids must be strictly increasing.
    pub fn push(&mut self, doc_id: DocId, vector: &[f32]) -> io::Result<()> {
        if vector.len() != self.dims {
            return Err(invalid_input(format!(
                "vector for doc {doc_id} has {} dims, expected {}",
                vector.len(),
                self.dims
            )));
        }
        if let Some(last) = self.last_doc
            && doc_id <= last
        {
            return Err(invalid_input(format!(
                "doc ids must be strictly increasing: {doc_id} after {last}"
            )));
        }
        if vector.iter().any(|v| !v.is_finite()) {
            return Err(invalid_input(format!(
                "vector for doc {doc_id} has a NaN or infinite component"
            )));
        }

        let ids = self.ids.as_mut().expect("present until finish");
        ids.write_all(&doc_id.to_le_bytes())?;
        let originals = self.originals.as_mut().expect("present until finish");
        for value in vector {
            originals.write_all(&value.to_le_bytes())?;
        }
        self.sample_row(vector);
        self.last_doc = Some(doc_id);
        self.count += 1;
        Ok(())
    }

    /// Reservoir sampling over rows (Algorithm R), so the range reflects the whole file at
    /// fixed memory. Deterministic per file.
    fn sample_row(&mut self, vector: &[f32]) {
        let held = self.sample.len() / self.dims;
        if held < self.sample_rows_cap {
            self.sample.extend_from_slice(vector);
            return;
        }
        let seen = self.count + 1;
        let slot = fmix64(self.seed ^ seen) % seen;
        if let Ok(slot) = usize::try_from(slot)
            && slot < self.sample_rows_cap
        {
            let start = slot * self.dims;
            self.sample[start..start + self.dims].copy_from_slice(vector);
        }
    }

    pub fn len(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Measures the range, assembles header, ids, codes and originals into `<path>.tmp`,
    /// fsyncs and atomically moves it into place. Returns the number of rows written.
    pub fn finish(mut self) -> io::Result<u64> {
        let mut ids = flush_to_file(self.ids.take().expect("present until finish"))?;
        let mut originals = flush_to_file(self.originals.take().expect("present until finish"))?;
        let count = usize::try_from(self.count).map_err(|_| invalid_input("too many rows"))?;
        let layout = Layout::new(self.dims, self.model_id.len(), count)
            .ok_or_else(|| invalid_input("vector file size overflows usize"))?;
        let range = QuantRange::learn(&mut self.sample);
        let file_id = new_file_id(&self.final_path);

        let tmp_path = tmp_path_for(&self.final_path);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        let mut out = BufWriter::new(file);

        out.write_all(&MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&file_id.to_le_bytes())?;
        out.write_all(&(self.dims as u32).to_le_bytes())?;
        out.write_all(&[ORIGINAL_F32, QUANT_INT8])?;
        out.write_all(&(self.model_id.len() as u16).to_le_bytes())?;
        out.write_all(&range.absmax().to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&self.count.to_le_bytes())?;
        out.write_all(self.model_id.as_bytes())?;
        write_zero_padding(&mut out, HEADER_FIXED_LEN + self.model_id.len(), layout.ids)?;

        ids.seek(SeekFrom::Start(0))?;
        io::copy(&mut ids, &mut out)?;
        write_zero_padding(&mut out, layout.ids + count * 8, layout.codes)?;

        originals.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut originals);
        let mut row_bytes = vec![0u8; self.dims * 4];
        let mut row = vec![0f32; self.dims];
        let mut codes = Vec::with_capacity(self.dims);
        for _ in 0..count {
            reader.read_exact(&mut row_bytes)?;
            for (value, bytes) in row.iter_mut().zip(row_bytes.chunks_exact(4)) {
                *value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            }
            range.encode(&row, &mut codes);
            out.write_all(&codes)?;
        }
        drop(reader);
        write_zero_padding(&mut out, layout.codes + count * self.dims, layout.originals)?;

        originals.seek(SeekFrom::Start(0))?;
        io::copy(&mut originals, &mut out)?;

        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        let written = file.metadata()?.len();
        if written != layout.total as u64 {
            return Err(io::Error::other(format!(
                "vector file {} assembled to {written} bytes, expected {}",
                tmp_path.display(),
                layout.total
            )));
        }
        drop(file);
        fs::rename(&tmp_path, &self.final_path)?;
        if let Some(dir) = self.final_path.parent() {
            File::open(dir)?.sync_all()?;
        }
        self.committed = true;
        Ok(self.count)
    }
}
impl Drop for VectorFileWriter {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.ids_path);
        let _ = fs::remove_file(&self.originals_path);
        if !self.committed {
            let _ = fs::remove_file(tmp_path_for(&self.final_path));
        }
    }
}
//gets file otu of BufWriter
fn flush_to_file(writer: BufWriter<File>) -> io::Result<File> {
    writer.into_inner().map_err(io::IntoInnerError::into_error)
}
//write zeros to fill the gap up to the next section so sections start at the same positon
pub(crate) fn write_zero_padding(out: &mut impl Write, from: usize, to: usize) -> io::Result<()> {
    const ZEROS: [u8; SECTION_ALIGNMENT] = [0; SECTION_ALIGNMENT];
    debug_assert!(to >= from && to - from < SECTION_ALIGNMENT, "write_zero_paddingding outside one alignment step");
    out.write_all(&ZEROS[..to - from])
}

/// Read-only, memory-mapped vector file.
#[derive(Debug)]
pub struct VectorFile {
    mmap: Mmap,
    path: PathBuf,
    file_id: u64,
    model_id: String,
    dims: usize,
    range: QuantRange,
    count: usize,
    layout: Layout,
}

impl VectorFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        // SAFETY: vector files are never modified after the rename in `finish`, and a
        // replacement is a new inode, so the mapping cannot change underneath us.
        let mmap = unsafe { Mmap::map(&file)? };
        let data = &mmap[..];

        if data.len() < HEADER_FIXED_LEN {
            return Err(corrupt(&path, "shorter than its header"));
        }
        if data[0..8] != MAGIC {
            return Err(corrupt(&path, "bad magic"));
        }
        let version = read_u32(data, 8);
        if version != VERSION {
            return Err(corrupt(&path, format!("unsupported version {version}")));
        }
        let file_id = read_u64(data, 16);
        let dims = read_u32(data, 24) as usize;
        if dims == 0 {
            return Err(corrupt(&path, "zero dimensions"));
        }
        if data[28] != ORIGINAL_F32 || data[29] != QUANT_INT8 {
            return Err(corrupt(
                &path,
                format!("unsupported encoding {}/{}", data[28], data[29]),
            ));
        }
        let id_len = usize::from(u16::from_le_bytes([data[30], data[31]]));
        let range = QuantRange::from_absmax(f32::from_le_bytes([
            data[32], data[33], data[34], data[35],
        ]))
        .ok_or_else(|| corrupt(&path, "invalid quantization range"))?;
        let count = usize::try_from(read_u64(data, 40))
            .map_err(|_| corrupt(&path, "record count does not fit in memory"))?;
        let id_end = HEADER_FIXED_LEN + id_len;
        if data.len() < id_end {
            return Err(corrupt(&path, "truncated header"));
        }
        let model_id = std::str::from_utf8(&data[HEADER_FIXED_LEN..id_end])
            .map_err(|e| corrupt(&path, format!("model id is not UTF-8: {e}")))?
            .to_string();
        let layout = Layout::new(dims, id_len, count)
            .ok_or_else(|| corrupt(&path, "size overflows usize"))?;
        if layout.total != data.len() {
            return Err(corrupt(
                &path,
                format!("length {} does not match {count} rows of {dims} dims", data.len()),
            ));
        }
        if (mmap.as_ptr() as usize) % SECTION_ALIGNMENT != 0 {
            return Err(corrupt(&path, "mapping is not aligned"));
        }

        let file = Self {
            mmap,
            path,
            file_id,
            model_id,
            dims,
            range,
            count,
            layout,
        };
        if file.doc_ids().windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(corrupt(&file.path, "doc ids are not strictly increasing"));
        }
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Random id of this written file; a graph built from it stores the same id.
    pub fn file_id(&self) -> u64 {
        self.file_id
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub(crate) fn range(&self) -> QuantRange {
        self.range
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

    /// All doc ids, strictly increasing; row `i` belongs to `doc_ids()[i]`.
    pub fn doc_ids(&self) -> &[DocId] {
        let bytes = &self.mmap[self.layout.ids..self.layout.ids + self.count * 8];
        // SAFETY: the section starts on a 64-byte boundary of a mapping checked to be 64-byte
        // aligned at open, holds exactly `count` little-endian u64s (length checked at open,
        // little-endian host enforced at compile time), and every bit pattern is a valid u64.
        #[allow(clippy::cast_ptr_alignment, reason = "alignment checked at open")]
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().cast::<DocId>(), self.count)
        }
    }

    pub fn doc_id(&self, index: usize) -> DocId {
        self.doc_ids()[index]
    }

    /// Row index of `doc_id`.
    pub fn position(&self, doc_id: DocId) -> Option<usize> {
        self.doc_ids().binary_search(&doc_id).ok()
    }

    pub fn contains(&self, doc_id: DocId) -> bool {
        self.position(doc_id).is_some()
    }

    /// Int8 codes of row `index`, as bytes.
    pub(crate) fn codes(&self, index: usize) -> &[u8] {
        let start = self.layout.codes + index * self.dims;
        &self.mmap[start..start + self.dims]
    }
     /// Original f32 vector of row `index`.
    pub fn original(&self, index: usize) -> &[f32] {
        assert!(index < self.count, "row {index} out of {} rows", self.count);
        let start = self.layout.originals + index * self.dims * 4;
        let bytes = &self.mmap[start..start + self.dims * 4];
        // SAFETY: the originals section starts on a 64-byte boundary of a 64-byte aligned
        // mapping, and every row is dims * 4 bytes, so each row start is 4-byte aligned. The
        // bytes are little-endian f32 (little-endian host enforced at compile time) and every
        // bit pattern is a valid f32.
        #[allow(clippy::cast_ptr_alignment, reason = "alignment checked at open")]
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), self.dims)
        }
    }
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn read_u64(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Merges vector files into one, dropping deleted documents. Used by compaction.
/// If the same document appears in several inputs, the row from the last input wins.
/// The output's range is measured again from the f32 originals, so quantization never drifts.
/// Returns the number of rows written. On failure no file is left at `output`.
pub fn merge_vector_files(
    inputs: &[&VectorFile],
    deleted: &DeleteSet,
    output: impl AsRef<Path>,
) -> io::Result<u64> {
    let first = inputs
        .first()
        .ok_or_else(|| invalid_input("merge_vector_files needs at least one input"))?;
    if let Some(other) = inputs.iter().find(|f| !f.matches(&first.model_id, first.dims)) {
        return Err(invalid_input(format!(
            "cannot merge {} ({}, {} dims) with {} ({}, {} dims)",
            other.path.display(),
            other.model_id,
            other.dims,
            first.path.display(),
            first.model_id,
            first.dims
        )));
    }

    let mut writer = VectorFileWriter::create(output, &first.model_id, first.dims)?;
    let mut cursors = vec![0usize; inputs.len()];
    loop {
        let next = inputs
            .iter()
            .zip(&cursors)
            .filter_map(|(file, &cursor)| file.doc_ids().get(cursor).copied())
            .min();
        let Some(doc) = next else { break };

        let mut source = None;
        for (n, file) in inputs.iter().enumerate() {
            if file.doc_ids().get(cursors[n]) == Some(&doc) {
                source = Some((n, cursors[n]));
                cursors[n] += 1;
            }
        }
        let (n, index) = source.expect("at least one input holds the selected doc id");
        if !deleted.contains(doc) {
            writer.push(doc, inputs[n].original(index))?;
        }
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::kernel::{approx_cosine, dot_f32, dot_i8, quantize_query};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corelamo-vfile-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn unit(seed: u64, dims: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dims as u64)
            .map(|i| (fmix64(seed ^ (i << 20) ^ 1) >> 40) as f32 / (1u64 << 24) as f32 - 0.5)
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    fn write(path: &Path, rows: &[(DocId, Vec<f32>)]) -> VectorFile {
        let mut writer = VectorFileWriter::create(path, "model", rows[0].1.len()).expect("create");
        for (doc, v) in rows {
            writer.push(*doc, v).expect("push");
        }
        writer.finish().expect("finish");
        VectorFile::open(path).expect("open")
    }

    #[test]
    fn round_trip_keeps_ids_and_originals() {
        let dir = temp_dir("roundtrip");
        let rows: Vec<(DocId, Vec<f32>)> = (1..=50).map(|d| (d * 3, unit(d, 37))).collect();
        let file = write(&dir.join("s.1.vec"), &rows);
        assert_eq!(file.len(), 50);
        assert_eq!(file.dims(), 37);
        for (i, (doc, v)) in rows.iter().enumerate() {
            assert_eq!(file.doc_id(i), *doc);
            assert_eq!(file.original(i), v.as_slice());
            assert_eq!(file.position(*doc), Some(i));
        }
        assert!(!file.contains(4));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codes_approximate_originals_at_high_dims() {
        let dir = temp_dir("codes");
        let rows: Vec<(DocId, Vec<f32>)> = (1..=200).map(|d| (d, unit(d, 4096))).collect();
        let file = write(&dir.join("s.1.vec"), &rows);
        let query = unit(9_999, 4096);
        let q = quantize_query(&query);
        for i in 0..file.len() {
            let exact = dot_f32(file.original(i), &query);
            let approx = approx_cosine(dot_i8(file.codes(i), &q.codes), file.range().scale(), q.scale);
            assert!((exact - approx).abs() < 0.02, "row {i}: exact {exact}, approx {approx}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn push_rejects_bad_rows() {
        let dir = temp_dir("reject");
        let mut writer = VectorFileWriter::create(dir.join("s.1.vec"), "m", 3).expect("create");
        writer.push(5, &[1.0, 0.0, 0.0]).expect("first row");
        assert!(writer.push(5, &[1.0, 0.0, 0.0]).is_err(), "duplicate id");
        assert!(writer.push(4, &[1.0, 0.0, 0.0]).is_err(), "decreasing id");
        assert!(writer.push(6, &[1.0, 0.0]).is_err(), "wrong dims");
        assert!(writer.push(7, &[f32::NAN, 0.0, 0.0]).is_err(), "NaN");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dropped_writer_leaves_no_files() {
        let dir = temp_dir("drop");
        let path = dir.join("s.1.vec");
        let mut writer = VectorFileWriter::create(&path, "m", 3).expect("create");
        writer.push(1, &[1.0, 0.0, 0.0]).expect("push");
        drop(writer);
        assert_eq!(fs::read_dir(&dir).expect("read dir").count(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn each_write_gets_a_new_file_id() {
        let dir = temp_dir("fileid");
        let rows = vec![(1, unit(1, 8))];
        let a = write(&dir.join("s.1.vec"), &rows).file_id();
        let b = write(&dir.join("s.1.vec"), &rows).file_id();
        assert_ne!(a, b);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_files_are_invalid_data() {
        let dir = temp_dir("corrupt");
        let path = dir.join("s.1.vec");
        write(&path, &[(1, unit(1, 8)), (2, unit(2, 8))]);
        let bytes = fs::read(&path).expect("read");

        fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");
        assert_eq!(VectorFile::open(&path).expect_err("truncated").kind(), io::ErrorKind::InvalidData);

        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&1u32.to_le_bytes());
        fs::write(&path, &old).expect("downgrade");
        assert_eq!(VectorFile::open(&path).expect_err("v1").kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn merge_drops_deleted_and_last_input_wins() {
        let dir = temp_dir("merge");
        let a = write(&dir.join("a.vec"), &[(1, unit(1, 8)), (2, unit(2, 8)), (3, unit(3, 8))]);
        let b = write(&dir.join("b.vec"), &[(2, unit(20, 8)), (4, unit(4, 8))]);
        let mut deleted = DeleteSet::new();
        deleted.delete(3);
        let out = dir.join("out.vec");
        assert_eq!(merge_vector_files(&[&a, &b], &deleted, &out).expect("merge"), 3);
        let merged = VectorFile::open(&out).expect("open");
        assert_eq!(merged.doc_ids(), &[1, 2, 4]);
        assert_eq!(merged.original(1), unit(20, 8).as_slice());
        let _ = fs::remove_dir_all(dir);
    }
}