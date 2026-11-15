use std::io::{self};
use bitpacking ::{BitPacker, BitPacker4x};
use crate::disk::format::{BLOCK_LEN,SkipEntry};

//ojne decoded block - id,freq,weight up to 128
pub struct DecodedBlock{
    pub docs:[u64;BLOCK_LEN],
    pub term_freq:[u32;BLOCK_LEN],
    pub weights:[u16;BLOCK_LEN],
    pub len:usize,

}
impl Default for DecodedBlock {
    fn default() -> Self {
        Self { docs: [0;BLOCK_LEN],
        term_freq:[0;BLOCK_LEN] , 
        weights: [0;BLOCK_LEN], 
        len: 0 }
    }
}

#[inline]
fn packed_len(bits: u8) -> usize {
    usize::from(bits) * BLOCK_LEN / 8
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unexpected_eof(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, message)
}
/// Encodes one block. A full block (128 docs) is bit-packed:
///
/// ```text
/// doc_bits u8, tf_bits u8, w_bits u8 (0 = every weight equals the skip entry's max_weight)
/// gaps     packed × 128   docs[i] - docs[i-1] - 1, gaps[0] = 0 (first_doc is in the skip entry)
/// tfs      packed × 128
/// weights  packed × 128   only when w_bits != 0
/// ```
///
/// A shorter block is varints:
///
/// ```text
/// has_weights u8
/// per doc: gap var u64 (not for the first doc), tf var u32, weight var u16 (when has_weights)
/// ```
///
/// The caller guarantees ids are strictly increasing and no gap inside the block exceeds `u32`.
pub fn encode_block(
    packer:&BitPacker4x,
    docs:&[u64],
    term_freq:&[u32],
    weights:&[u16],
    out:&mut Vec<u8>
){
    let len = docs.len();
    debug_assert!(len > 0 && len <= BLOCK_LEN);
    debug_assert!(term_freq.len() == len && weights.len() == len);
    debug_assert!(docs.windows(2).all(|pair| pair[0] < pair[1]));

    let uniform_weights = weights.iter().all(|&weight| weight == weights[0]);

    if len == BLOCK_LEN {
        encode_packed(packer, docs, term_freq, weights, uniform_weights, out);
    } else {
        encode_varint(docs, term_freq, weights, uniform_weights, out);
    }
}

/// Decodes the block described by `entry`. `bytes` starts at the block (the term's blocks area
/// sliced at `entry.block_start`). Returns how many bytes the block used.
pub fn decode_block(
    packer: &BitPacker4x,
    bytes: &[u8],
    entry: &SkipEntry,
    out: &mut DecodedBlock,
) -> io::Result<usize> {
    let len = usize::from(entry.len);
    let used = if len == BLOCK_LEN {
        decode_packed(packer, bytes, entry, out)?
    } else {
        decode_varint(bytes, entry, len, out)?
    };

    if out.docs[len - 1] != entry.last_doc_id {
        return Err(invalid_data("block last doc does not match skip entry"));
    }
    out.len = len;
    Ok(used)
}
fn encode_packed(
    packer: &BitPacker4x,
    docs: &[u64],
    tfs: &[u32],
    weights: &[u16],
    uniform_weights: bool,
    out: &mut Vec<u8>,
) {
    let mut values = [0u32; BLOCK_LEN];

    values[0] = 0;
    for i in 1..BLOCK_LEN {
        let gap = docs[i] - docs[i - 1] - 1;
        values[i] = u32::try_from(gap).expect("BlockWriter splits blocks at gaps over u32");
    }

    let header = out.len();
    out.extend_from_slice(&[0, 0, 0]);

    out[header] = pack_into(packer, &values, out);

    values.copy_from_slice(tfs);
    out[header + 1] = pack_into(packer, &values, out);

    if !uniform_weights {
        for (value, &weight) in values.iter_mut().zip(weights) {
            *value = u32::from(weight);
        }
        out[header + 2] = pack_into(packer, &values, out);
    }
}

/// Packs 128 values at the smallest width that fits them all; returns that width.
fn pack_into(packer: &BitPacker4x, values: &[u32; BLOCK_LEN], out: &mut Vec<u8>) -> u8 {
    let bits = packer.num_bits(values);
    if bits == 0 {
        return 0;
    }
    let start = out.len();
    out.resize(start + packed_len(bits), 0);
    packer.compress(values, &mut out[start..], bits);
    bits
}

fn encode_varint(
    docs: &[u64],
    tfs: &[u32],
    weights: &[u16],
    uniform_weights: bool,
    out: &mut Vec<u8>,
) {
    out.push(u8::from(!uniform_weights));

    for i in 0..docs.len() {
        if i > 0 {
            push_var_u64(out, docs[i] - docs[i - 1] - 1);
        }
        push_var_u32(out, tfs[i]);
        if !uniform_weights {
            push_var_u16(out, weights[i]);
        }
    }
}



fn decode_packed(
    packer: &BitPacker4x,
    bytes: &[u8],
    entry: &SkipEntry,
    out: &mut DecodedBlock,
) -> io::Result<usize> {
    let header = bytes
        .get(..3)
        .ok_or_else(|| unexpected_eof("missing block header"))?;
    let (doc_bits, tf_bits, weight_bits) = (header[0], header[1], header[2]);

    if doc_bits > 32 || tf_bits > 32 || weight_bits > 16 {
        return Err(invalid_data("block bit width out of range"));
    }

    let mut at = 3;
    let mut values = [0u32; BLOCK_LEN];

    unpack(packer, bytes, &mut at, doc_bits, &mut values)?;
    let mut doc = entry.first_doc_id;
    out.docs[0] = doc;
    for i in 1..BLOCK_LEN {
        doc = doc
            .checked_add(u64::from(values[i]) + 1)
            .ok_or_else(|| invalid_data("doc id overflow in block"))?;
        out.docs[i] = doc;
    }

    unpack(packer, bytes, &mut at, tf_bits, &mut values)?;
    out.term_freq.copy_from_slice(&values);

    if weight_bits == 0 {
        out.weights.fill(entry.max_weight);
    } else {
        unpack(packer, bytes, &mut at, weight_bits, &mut values)?;
        for (weight, &value) in out.weights.iter_mut().zip(&values) {
            *weight = u16::try_from(value).map_err(|_| invalid_data("weight does not fit u16"))?;
        }
    }

    Ok(at)
}

fn unpack(
    packer: &BitPacker4x,
    bytes: &[u8],
    at: &mut usize,
    bits: u8,
    values: &mut [u32; BLOCK_LEN],
) -> io::Result<()> {
    if bits == 0 {
        values.fill(0);
        return Ok(());
    }
    let len = packed_len(bits);
    let source = bytes
        .get(*at..*at + len)
        .ok_or_else(|| unexpected_eof("packed block truncated"))?;
    packer.decompress(source, values, bits);
    *at += len;
    Ok(())
}

pub(crate) fn decode_varint(
    bytes: &[u8],
    entry: &SkipEntry,
    len: usize,
    out: &mut DecodedBlock,
) -> io::Result<usize> {
    let has_weights = *bytes
        .first()
        .ok_or_else(|| unexpected_eof("missing block header"))?
        != 0;

    let mut at = 1;
    let mut doc = entry.first_doc_id;

    for i in 0..len {
        if i > 0 {
            let gap = read_var_u64(bytes, &mut at)?;
            doc = gap
                .checked_add(1)
                .and_then(|step| doc.checked_add(step))
                .ok_or_else(|| invalid_data("doc id overflow in block"))?;
        }
        out.docs[i] = doc;
        out.term_freq[i] = read_var_u32(bytes, &mut at)?;
        out.weights[i] = if has_weights {
            read_var_u16(bytes, &mut at)?
        } else {
            entry.max_weight
        };
    }

    Ok(at)
}

pub fn skip_varints(input: &[u8], offset: &mut usize, count: usize) -> io::Result<()> {
    for _ in 0..count {
        loop {
            let byte = *input.get(*offset).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected eof while skipping varint")
            })?;
            *offset += 1;
            if byte & 0x80 == 0 {
                break;
            }
        }
    }
    Ok(())
}


pub fn push_var_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }

    out.push(value as u8);
}

pub fn push_var_u32(out: &mut Vec<u8>, value: u32) {
    push_var_u64(out, u64::from(value));
}

pub fn push_var_u16(out: &mut Vec<u8>, value: u16) {
    push_var_u64(out, u64::from(value));
}

pub fn read_var_u64(input: &[u8], offset: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;

    loop {
        if *offset >= input.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected eof while reading varint",
            ));
        }

        let byte = input[*offset];
        *offset += 1;

        value |= u64::from(byte & 0x7f) << shift;

        if byte & 0x80 == 0 {
            return Ok(value);
        }

        shift += 7;

        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "varint too large",
            ));
        }
    }
}

pub fn read_var_u32(input: &[u8], offset: &mut usize) -> io::Result<u32> {
    let value = read_var_u64(input, offset)?;

    u32::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "varint does not fit into u32"))
}

pub fn read_var_u16(input: &[u8], offset: &mut usize) -> io::Result<u16> {
    let value = read_var_u64(input, offset)?;

    u16::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "varint does not fit into u16"))
}
