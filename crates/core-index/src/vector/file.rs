//! Vector file: the vectors of one vector field for the documents of one segment.
//!
//! Layout (little-endian), every section starting on a 64-byte boundary:
//! +-------------+
//! | header      |  magic[8] "CLVECTOR", u32 version, u32 reserved, u64 `file_id`, u32 dims,
//! |             |  u8 `original_kind`, u8 `quant_kind`, u16 `model_id_len`, f32 `quant_absmax`,
//! |             |  u32 reserved, u64 count, `model_id` bytes
//! | doc ids     |  u64 * count, in row order (the order rows were pushed)
//! | int8 codes  |  i8 * dims * count   hot: scans and graph walks
//! | originals   |  f32 * dims * count  cold: rescoring, compaction, graph build
//! | lookup      |  u32 * count, rows ordered by doc id (binary search); version 3 only
//!
//! Vectors must be L2-normalized, so dot product = cosine. Codes are
//! round(x * 127 / `quant_absmax`), clipped; `file_id` is random per written file and is what a
//! graph binds to. Files are written to `<path>.tmp`, fsynced and renamed, and never modified.
#[cfg(target_endian = "big")]
compile_error!("vector files are mapped as little-endian slices");

use std::fs::{ self, File, OpenOptions };
use std::io::{ self, BufReader, BufWriter, Read, Seek, SeekFrom, Write };
use std::path::{ Path, PathBuf };
use std::sync::atomic::{ AtomicU64, Ordering };
use std::time::{ SystemTime, UNIX_EPOCH };
use crate::disk::format::array_at;
use memmap2::Mmap;

use super::kernel::QuantRange;
use crate::hash::{ fmix64, stable_hash64 };
use crate::posting::DeleteSet;
use crate::{ tmp_path_for, with_suffix };
use crate::types::{ DocId, XPathId };

pub(super) const MAGIC: [u8; 8] = *b"CLVECTOR";
pub(super) const VERSION: u32 = 3;

const ORIGINAL_F32: u8 = 1;
const QUANT_INT8: u8 = 1;
const MAX_MODEL_ID_LEN: usize = u8::MAX as usize;
/// magic + version + reserved + `file_id` + dims + `original_kind` + `quant_kind` + `model_id_len` +
/// `quant_absmax` + reserved + count
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
        if Path::new(rest)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp")) {
            drop(fs::remove_file(entry.path()));
            continue;
        }
        if let Some(xpath) = rest.strip_suffix(".vec").and_then(|x| x.parse::<XPathId>().ok()) {
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
//the rows embedded since the last
/// fold into the main file.
pub fn tail_file_path(segment_path: &Path, xpath: XPathId) -> PathBuf{
    segment_path.with_extension(format!("{xpath}.tail.vec"))
}

pub(crate) fn invalid_input(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn corrupt(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("vector file {}: {msg}", path.display()))
}

fn header_field<const N: usize>(data: &[u8], at: usize, path: &Path) -> io::Result<[u8; N]> {
    array_at(data, at).ok_or_else(|| corrupt(path, "shorter than its header"))
}
//round a positon up to the next multiple of 64
fn round_up_to_section(value: usize) -> Option<usize> {
    value.checked_add(SECTION_ALIGNMENT - 1).map(|v| (v / SECTION_ALIGNMENT) * SECTION_ALIGNMENT)
}

/// Byte offsets of every section. The one definition shared by writer and reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    ids: usize,
    codes: usize,
    originals: usize,
    lookup: usize,
    total: usize,
}

impl Layout {
    fn new(dims: usize, model_id_len: usize, count: usize) -> Option<Self> {
        let ids = round_up_to_section(HEADER_FIXED_LEN.checked_add(model_id_len)?)?;
        let codes = round_up_to_section(ids.checked_add(count.checked_mul(8)?)?)?;
        let originals = round_up_to_section(codes.checked_add(count.checked_mul(dims)?)?)?;
        let lookup = round_up_to_section(
            originals.checked_add(count.checked_mul(dims)?.checked_mul(4)?)?,
        )?;
        let total = lookup.checked_add(count.checked_mul(4)?)?;
        Some(Self {
            ids,
            codes,
            originals,
            lookup,
            total,
        })
    }
}

/// Random per written file; collisions only need to be unlikely between the files of one
/// segment and field over its lifetime.
fn new_file_id(path: &Path) -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH) //maybe needs to be universal
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let process = u64::from(std::process::id());
    fmix64(
        nanos ^
            fmix64(process.rotate_left(32) ^ sequence) ^
            stable_hash64(path.as_os_str().as_encoded_bytes())
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
    sample: Vec<f32>,
    sample_rows_cap: usize,
    seed: u64,
    committed: bool,
}

impl VectorFileWriter {
    pub fn create(path: impl AsRef<Path>, model_id: &str, dims: usize) -> io::Result<Self> {
        if dims == 0 || dims > (u32::MAX as usize) {
            return Err(invalid_input(format!("invalid vector dimension {dims}")));
        }
        if model_id.is_empty() || model_id.len() > MAX_MODEL_ID_LEN {
            return Err(
                invalid_input(
                    format!("model id must be 1..={MAX_MODEL_ID_LEN} bytes, got {}", model_id.len())
                )
            );
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
                drop(fs::remove_file(&ids_path));
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
            model_id: model_id.to_owned(),
            dims,
            count: 0,
            sample: Vec::with_capacity(sample_rows_cap.saturating_mul(dims).min(SAMPLE_COMPONENTS)),
            sample_rows_cap,
            committed: false,
        })
    }
    /// Appends one L2-normalized vector. Doc ids must be strictly increasing.
        pub fn push(&mut self, doc_id: DocId, vector: &[f32]) -> io::Result<()> {
        if vector.len() != self.dims {
            return Err(
                invalid_input(
                    format!(
                        "vector for doc {doc_id} has {} dims, expected {}",
                        vector.len(),
                        self.dims
                    )
                )
            );
        }
        if self.count >= u64::from(u32::MAX) {
            return Err(invalid_input(format!("a vector file holds at most {} rows", u32::MAX)));
        }
        if vector.iter().any(|v| !v.is_finite()) {
            return Err(
                invalid_input(format!("vector for doc {doc_id} has a NaN or infinite component"))
            );
        }

        let (Some(ids), Some(originals)) = (self.ids.as_mut(), self.originals.as_mut()) else {
            return Err(io::Error::other("vector file writer already finished"));
        };
        ids.write_all(&doc_id.to_le_bytes())?;
        for value in vector {
            originals.write_all(&value.to_le_bytes())?;
        }
        self.sample_row(vector);
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
        if let Ok(slot) = usize::try_from(slot) && slot < self.sample_rows_cap {
            let start = slot * self.dims;
            if let Some(row) = self.sample.get_mut(start..start + self.dims) {
                row.copy_from_slice(vector);
            }
        }
    }

    pub const fn len(&self) -> u64 {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Measures the range, assembles header, ids, codes and originals into `<path>.tmp`,
    /// fsyncs and atomically moves it into place. Returns the number of rows written.
        pub fn finish(mut self) -> io::Result<u64> {
        let (Some(ids_writer), Some(originals_writer)) = (self.ids.take(), self.originals.take())
        else {
            return Err(io::Error::other("vector file writer already finished"));
        };
        let mut ids = flush_to_file(ids_writer)?;
        let mut originals = flush_to_file(originals_writer)?;
        let count = usize::try_from(self.count).map_err(|_overflow| invalid_input("too many rows"))?;
        let layout = Layout::new(self.dims, self.model_id.len(), count).ok_or_else(||
            invalid_input("vector file size overflows usize")
        )?;
        ids.seek(SeekFrom::Start(0))?;
        let mut id_bytes = Vec::with_capacity(count * 8);
        (&mut ids).take(self.count * 8).read_to_end(&mut id_bytes)?;
        let lookup = sorted_lookup(&id_bytes)?;
        let range = QuantRange::learn(&mut self.sample);
        let file_id = new_file_id(&self.final_path);
        let dims = u32::try_from(self.dims).map_err(|_overflow| invalid_input("dims exceed u32"))?;
        let model_id_len = u16::try_from(self.model_id.len())
            .map_err(|_overflow| invalid_input("model id too long"))?;

        let tmp_path = tmp_path_for(&self.final_path);
        let tmp_file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp_path)?;
        let mut out = BufWriter::new(tmp_file);

        out.write_all(&MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&(0u32).to_le_bytes())?;
        out.write_all(&file_id.to_le_bytes())?;
        out.write_all(&dims.to_le_bytes())?;
        out.write_all(&[ORIGINAL_F32, QUANT_INT8])?;
        out.write_all(&model_id_len.to_le_bytes())?;
        out.write_all(&range.absmax().to_le_bytes())?;
        out.write_all(&(0u32).to_le_bytes())?;
        out.write_all(&self.count.to_le_bytes())?;
        out.write_all(self.model_id.as_bytes())?;
        write_zero_padding(&mut out, HEADER_FIXED_LEN + self.model_id.len(), layout.ids)?;

        out.write_all(&id_bytes)?;
        write_zero_padding(&mut out, layout.ids + count * 8, layout.codes)?;

        originals.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut originals);
        let mut row_bytes = vec![0u8; self.dims * 4];
        let mut row = vec![0f32; self.dims];
        let mut codes = Vec::with_capacity(self.dims);
        for _ in 0..count {
            reader.read_exact(&mut row_bytes)?;
            let (chunks, _) = row_bytes.as_chunks::<4>();
            for (value, bytes) in row.iter_mut().zip(chunks) {
                *value = f32::from_le_bytes(*bytes);
            }
            range.encode(&row, &mut codes);
            out.write_all(&codes)?;
        }
        drop(reader);
        write_zero_padding(&mut out, layout.codes + count * self.dims, layout.originals)?;

        originals.seek(SeekFrom::Start(0))?;
        io::copy(&mut originals, &mut out)?;
        write_zero_padding(&mut out, layout.originals + count * self.dims * 4, layout.lookup)?;
        for lookup_row in &lookup {
            out.write_all(&lookup_row.to_le_bytes())?;
        }
        let finished = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        finished.sync_all()?;
        let written = finished.metadata()?.len();
        if written != (layout.total as u64) {
            return Err(
                io::Error::other(
                    format!(
                        "vector file {} assembled to {written} bytes, expected {}",
                        tmp_path.display(),
                        layout.total
                    )
                )
            );
        }
        drop(finished);
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
        drop(fs::remove_file(&self.ids_path));
        drop(fs::remove_file(&self.originals_path));
        if !self.committed {
            drop(fs::remove_file(tmp_path_for(&self.final_path)));
        }
    }
}
//gets file otu of BufWriter
fn flush_to_file(writer: BufWriter<File>) -> io::Result<File> {
    writer.into_inner().map_err(io::IntoInnerError::into_error)
}
/// Row indices ordered by doc id. Fails naming the first doc id that appears twice.
fn sorted_lookup(id_bytes: &[u8]) -> io::Result<Vec<u32>> {
    let (chunks, _) = id_bytes.as_chunks::<8>();
    let ids: Vec<DocId> = chunks.iter().map(|bytes| DocId::from_le_bytes(*bytes)).collect();
    let rows = u32::try_from(ids.len()).map_err(|_overflow| invalid_input("too many rows"))?;
    // each row travels with its doc id, so nothing is looked up by index
    let mut keyed: Vec<(DocId, u32)> = ids.iter().copied().zip(0..rows).collect();
    keyed.sort_unstable();
    let duplicate = keyed.windows(2).find_map(|pair| match pair {
        [first, second] if first.0 == second.0 => Some(first.0),
        _ => None,
    });
    if let Some(doc_id) = duplicate {
        return Err(invalid_input(format!("doc id {doc_id} is written twice")));
    }
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}
//write zeros to fill the gap up to the next section so sections start at the same positon
pub(crate) fn write_zero_padding(out: &mut impl Write, from: usize, to: usize) -> io::Result<()> {
    const ZEROS: [u8; SECTION_ALIGNMENT] = [0; SECTION_ALIGNMENT];
    let padding = to
        .checked_sub(from)
        .and_then(|len| ZEROS.get(..len))
        .ok_or_else(|| invalid_input("padding outside one alignment step"))?;
    out.write_all(padding)
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
    layout: Layout
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
        if data.first_chunk::<8>() != Some(&MAGIC) {
            return Err(corrupt(&path, "bad magic"));
        }
        let version = u32::from_le_bytes(header_field(data, 8, &path)?);
        if version != VERSION {
            return Err(corrupt(&path, format!("unsupported version {version}")));
        }
        let file_id = u64::from_le_bytes(header_field(data, 16, &path)?);
        let dims = u32::from_le_bytes(header_field(data, 24, &path)?) as usize;
        if dims == 0 {
            return Err(corrupt(&path, "zero dimensions"));
        }
        let [original_kind, quant_kind] = header_field::<2>(data, 28, &path)?;
        if original_kind != ORIGINAL_F32 || quant_kind != QUANT_INT8 {
            return Err(
                corrupt(&path, format!("unsupported encoding {original_kind}/{quant_kind}"))
            );
        }
        let id_len = usize::from(u16::from_le_bytes(header_field(data, 30, &path)?));
        let range = QuantRange::from_absmax(f32::from_le_bytes(header_field(data, 32, &path)?))
            .ok_or_else(|| corrupt(&path, "invalid quantization range"))?;
        let count = usize::try_from(u64::from_le_bytes(header_field(data, 40, &path)?))
            .map_err(|_overflow| corrupt(&path, "record count does not fit in memory"))?;
        let id_end = HEADER_FIXED_LEN + id_len;
        let model_id_bytes = data
            .get(HEADER_FIXED_LEN..id_end)
            .ok_or_else(|| corrupt(&path, "truncated header"))?;
        let model_id = std::str
            ::from_utf8(model_id_bytes)
            .map_err(|e| corrupt(&path, format!("model id is not UTF-8: {e}")))?
            .to_owned();
        let layout = Layout::new(dims, id_len, count).ok_or_else(||
            corrupt(&path, "size overflows usize")
        )?;
        if layout.total != data.len() {
            return Err(
                corrupt(
                    &path,
                    format!("length {} does not match {count} rows of {dims} dims", data.len())
                )
            );
        }
        if !mmap.as_ptr().addr().is_multiple_of(SECTION_ALIGNMENT) {
            return Err(corrupt(&path, "mapping is not aligned"));
        }

        let opened = Self {
            mmap,
            path,
            file_id,
            model_id,
            dims,
            range,
            count,
            layout
        };
        // strictly increasing ids through the lookup also proves every row is listed once
        let ids = opened.doc_ids();
        let mut previous: Option<DocId> = None;
        for &row in opened.lookup() {
            let Some(&id) = ids.get(row as usize) else {
                return Err(corrupt(&opened.path, "lookup row out of range"));
            };
            if previous.is_some_and(|before| before >= id) {
                return Err(corrupt(&opened.path, "lookup is not ordered by doc id"));
            }
            previous = Some(id);
        }
        Ok(opened)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Random id of this written file; a graph built from it stores the same id.
    pub const fn file_id(&self) -> u64 {
        self.file_id
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub const fn dims(&self) -> usize {
        self.dims
    }

    pub(crate) const fn range(&self) -> QuantRange {
        self.range
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// False when the file was written by a different model and must be re-embedded.
    pub fn matches(&self, model_id: &str, dims: usize) -> bool {
        self.model_id == model_id && self.dims == dims
    }

    /// All doc ids, strictly increasing; row `i` belongs to `doc_ids()[i]`.
        pub fn doc_ids(&self) -> &[DocId] {
        let Some(bytes) = self.mmap.get(self.layout.ids..self.layout.ids + self.count * 8) else {
            return &[];
        };
        // SAFETY: (keep your existing comment)
        #[allow(clippy::cast_ptr_alignment, reason = "alignment checked at open")]
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().cast::<DocId>(), self.count)
        }
    }

    #[allow(clippy::indexing_slicing, reason = "rows come from this file's own lookup; an out of range row is a caller bug, as in `original`")]
    pub fn doc_id(&self, index: usize) -> DocId {
        self.doc_ids()[index]
    }

    fn lookup(&self) -> &[u32] {
        let Some(bytes) = self.mmap.get(self.layout.lookup..self.layout.lookup + self.count * 4)
        else {
            return &[];
        };
        // SAFETY: (keep your existing comment)
        #[allow(clippy::cast_ptr_alignment, reason = "alignment checked at open")]
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().cast::<u32>(), self.count)
        }
    }

    /// Row index of `doc_id`.
    pub fn position(&self, doc_id: DocId) -> Option<usize> {
        let ids = self.doc_ids();
        let lookup = self.lookup();
        let rank = lookup
            .binary_search_by_key(&doc_id, |&row| {
                ids.get(row as usize).copied().unwrap_or(DocId::MAX)
            })
            .ok()?;
        lookup.get(rank).map(|&row| row as usize)
    }

    /// Row holding the `rank`-th smallest doc id, for walking a file in doc-id order.
    pub fn row_by_rank(&self, rank: usize) -> Option<usize> {
        self.lookup().get(rank).map(|&row| row as usize)
    }

    pub fn contains(&self, doc_id: DocId) -> bool {
        self.position(doc_id).is_some()
    }

    /// Int8 codes of row `index`, as bytes.
    pub(crate) fn codes(&self, index: usize) -> &[u8] {
        let start = self.layout.codes + index * self.dims;
        self.mmap.get(start..start + self.dims).unwrap_or_default()
    }

    pub fn original(&self, index: usize) -> &[f32] {
        assert!(index < self.count, "row {index} out of {} rows", self.count);
        let start = self.layout.originals + index * self.dims * 4;
        let Some(bytes) = self.mmap.get(start..start + self.dims * 4) else {
            return &[];
        };
        // SAFETY: (keep your existing comment)
        #[allow(clippy::cast_ptr_alignment, reason = "alignment checked at open")]
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), self.dims)
        }
    }
}




pub fn merge_vector_files(
    inputs: &[&VectorFile],
    deleted: &DeleteSet,
    output: impl AsRef<Path>
) -> io::Result<u64> {
    let first = inputs
        .first()
        .ok_or_else(|| invalid_input("merge_vector_files needs at least one input"))?;
    if let Some(other) = inputs.iter().find(|f| !f.matches(&first.model_id, first.dims)) {
        return Err(
            invalid_input(
                format!(
                    "cannot merge {} ({}, {} dims) with {} ({}, {} dims)",
                    other.path.display(),
                    other.model_id,
                    other.dims,
                    first.path.display(),
                    first.model_id,
                    first.dims
                )
            )
        );
    }

    let mut writer = VectorFileWriter::create(output, &first.model_id, first.dims)?;
    let mut cursors = vec![0usize; inputs.len()];
    loop {
        let next = inputs
            .iter()
            .zip(&cursors)
            .filter_map(|(file, &rank)| file.row_by_rank(rank).map(|row| file.doc_id(row)))
            .min();
        let Some(doc) = next else {
            break;
        };

        let mut source = None;
        for (file, cursor) in inputs.iter().zip(cursors.iter_mut()) {
            if let Some(row) = file.row_by_rank(*cursor) && file.doc_id(row) == doc {
                source = Some((file, row));
                *cursor += 1;
            }
        }
        let Some((file, row)) = source else {
            return Err(io::Error::other("selected doc id vanished from its input"));
        };
        if !deleted.contains(doc) {
            writer.push(doc, file.original(row))?;
        }
    }
    writer.finish()
}

