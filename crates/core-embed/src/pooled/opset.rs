//! Reads the `ai.onnx` opset version from an ONNX file. Nodes added to a model must use the
//! model's own opset version, and ONNX Runtime does not report it, so it is read from the
//! file's protobuf directly.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
use std::path::Path;

/// The `ai.onnx` opset an ONNX file declares. Reads the top-level fields of the `ModelProto`
/// and seeks past the graph, so a large model costs a few small reads.
pub(super) fn onnx_opset(path: &Path) -> io::Result<u32> {
    /// `ModelProto.opset_import`
    const OPSET_IMPORT: u64 = 8;
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    while reader.stream_position()? < len {
        let key = read_varint(&mut reader)?;
        match (key >> 3, key & 7) {
            (OPSET_IMPORT, 2) => {
                let size = usize::try_from(read_varint(&mut reader)?)
                    .map_err(|_| invalid_onnx("opset entry too large"))?;
                let mut entry = vec![0_u8; size];
                reader.read_exact(&mut entry)?;
                if let Some(version) = default_domain_version(&entry)? {
                    return Ok(version);
                }
            }
            (_, 0) => {
                read_varint(&mut reader)?;
            }
            (_, 1) => reader.seek_relative(8)?,
            (_, 2) => {
                let size = i64::try_from(read_varint(&mut reader)?)
                    .map_err(|_| invalid_onnx("field too large"))?;
                reader.seek_relative(size)?;
            }
            (_, 5) => reader.seek_relative(4)?,
            (_, wire_type) => {
                return Err(invalid_onnx(&format!("unsupported wire type {wire_type}")));
            }
        }
    }
    Err(invalid_onnx("the file declares no ai.onnx opset"))
}

/// The version in one `OperatorSetIdProto` if its domain is `ai.onnx` (written as "" or
/// "ai.onnx").
fn default_domain_version(mut entry: &[u8]) -> io::Result<Option<u32>> {
    let mut domain: &[u8] = b"";
    let mut version = None;
    while !entry.is_empty() {
        let key = read_varint(&mut entry)?;
        match (key >> 3, key & 7) {
            (1, 2) => {
                let size = usize::try_from(read_varint(&mut entry)?)
                    .map_err(|_| invalid_onnx("domain too long"))?;
                if size > entry.len() {
                    return Err(invalid_onnx("truncated opset entry"));
                }
                (domain, entry) = entry.split_at(size);
            }
            (2, 0) => version = Some(read_varint(&mut entry)?),
            (_, 0) => {
                read_varint(&mut entry)?;
            }
            (_, 2) => {
                let size = usize::try_from(read_varint(&mut entry)?)
                    .map_err(|_| invalid_onnx("field too large"))?;
                if size > entry.len() {
                    return Err(invalid_onnx("truncated opset entry"));
                }
                entry = &entry[size..];
            }
            _ => return Err(invalid_onnx("unexpected field in opset entry")),
        }
    }
    match domain {
        b"" | b"ai.onnx" => version
            .map(|version| u32::try_from(version).map_err(|_| invalid_onnx("opset out of range")))
            .transpose(),
        _ => Ok(None),
    }
}

fn read_varint(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid_onnx("varint longer than 10 bytes"))
}

fn invalid_onnx(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a valid ONNX model: {reason}"),
    )
}

