use std::io;

use bitpacking::{ BitPacker, BitPacker4x };

use crate::disk::format::{ BLOCK_LEN, SkipEntry };

/// One decoded block: ids, term frequencies and weights, up to 128 entries.
pub struct DecodedBlock {
    pub docs: [u64; BLOCK_LEN],
    pub term_freq: [u32; BLOCK_LEN],
    pub weights: [u16; BLOCK_LEN],
    pub len: usize,
}

impl Default for DecodedBlock {
    fn default() -> Self {
        Self {
            docs: [0; BLOCK_LEN],
            term_freq: [0; BLOCK_LEN],
            weights: [0; BLOCK_LEN],
            len: 0,
        }
    }
}

#[inline]
fn packed_len(bits: u8) -> usize {
    usize::from(bits) * BLOCK_LEN / 8
}

fn invalid_data(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unexpected_eof(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, message)
}

/// Distance between two doc ids minus one; errors if they are not strictly increasing.
fn gap_between(previous: u64, next: u64) -> io::Result<u64> {
    next.checked_sub(previous)
        .and_then(|distance| distance.checked_sub(1))
        .ok_or_else(|| invalid_data("doc ids are not strictly increasing"))
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
/// The caller guarantees ids are strictly increasing and no gap inside the block exceeds `u32`;
/// a violation is reported as an error.
pub fn encode_block(
    packer: BitPacker4x,
    docs: &[u64],
    term_freq: &[u32],
    weights: &[u16],
    out: &mut Vec<u8>
) -> io::Result<()> {
    let len = docs.len();
    debug_assert!((1..=BLOCK_LEN).contains(&len));
    debug_assert!(term_freq.len() == len && weights.len() == len);

    let uniform_weights = weights
        .split_first()
        .is_none_or(|(&first, rest)| rest.iter().all(|&weight| weight == first));

    if len == BLOCK_LEN {
        encode_packed(packer, docs, term_freq, weights, uniform_weights, out)
    } else {
        encode_varint(docs, term_freq, weights, uniform_weights, out)
    }
}

/// Decodes the block described by `entry`. `bytes` starts at the block (the term's blocks area
/// sliced at `entry.block_start`). Returns how many bytes the block used.
pub(crate) fn decode_block(
    packer: BitPacker4x,
    bytes: &[u8],
    entry: &SkipEntry,
    out: &mut DecodedBlock
) -> io::Result<usize> {
    let len = usize::from(entry.len);
    if len == 0 || len > BLOCK_LEN {
        return Err(invalid_data("block length out of range"));
    }
    let used = if len == BLOCK_LEN {
        decode_packed(packer, bytes, entry, out)?
    } else {
        decode_varint(bytes, entry, len, out)?
    };

    if out.docs.get(len - 1).copied() != Some(entry.last_doc_id) {
        return Err(invalid_data("block last doc does not match skip entry"));
    }
    out.len = len;
    Ok(used)
}

fn encode_packed(
    packer: BitPacker4x,
    docs: &[u64],
    tfs: &[u32],
    weights: &[u16],
    uniform_weights: bool,
    out: &mut Vec<u8>
) -> io::Result<()> {
    let mut values = [0u32; BLOCK_LEN];
    let pairs = docs.iter().zip(docs.iter().skip(1));
    for (value, (&previous, &next)) in values.iter_mut().skip(1).zip(pairs) {
        let gap = gap_between(previous, next)?;
        *value = u32::try_from(gap).map_err(|e|
            invalid_data(format!("doc id gap does not fit u32: {e}"))
        )?;
    }

    let header = out.len();
    out.extend_from_slice(&[0, 0, 0]);

    let doc_bits = pack_into(packer, &values, out);

    values.copy_from_slice(tfs);
    let tf_bits = pack_into(packer, &values, out);

    let weight_bits = if uniform_weights {
        0
    } else {
        for (value, &weight) in values.iter_mut().zip(weights) {
            *value = u32::from(weight);
        }
        pack_into(packer, &values, out)
    };

    if let Some([doc_slot, tf_slot, weight_slot]) = out.get_mut(header..header + 3) {
        *doc_slot = doc_bits;
        *tf_slot = tf_bits;
        *weight_slot = weight_bits;
    }
    Ok(())
}

/// Packs 128 values at the smallest width that fits them all; returns that width.
fn pack_into(packer: BitPacker4x, values: &[u32; BLOCK_LEN], out: &mut Vec<u8>) -> u8 {
    let bits = packer.num_bits(values);
    if bits == 0 {
        return 0;
    }
    let start = out.len();
    out.resize(start + packed_len(bits), 0);
    if let Some(destination) = out.get_mut(start..) {
        packer.compress(values, destination, bits);
    }
    bits
}

fn encode_varint(
    docs: &[u64],
    tfs: &[u32],
    weights: &[u16],
    uniform_weights: bool,
    out: &mut Vec<u8>
) -> io::Result<()> {
    out.push(u8::from(!uniform_weights));

    let mut previous: Option<u64> = None;
    for ((&doc, &tf), &weight) in docs.iter().zip(tfs).zip(weights) {
        if let Some(prev) = previous {
            push_var_u64(out, gap_between(prev, doc)?);
        }
        previous = Some(doc);
        push_var_u32(out, tf);
        if !uniform_weights {
            push_var_u16(out, weight);
        }
    }
    Ok(())
}

fn decode_packed(
    packer: BitPacker4x,
    bytes: &[u8],
    entry: &SkipEntry,
    out: &mut DecodedBlock
) -> io::Result<usize> {
    let Some(&[doc_bits, tf_bits, weight_bits]) = bytes.first_chunk::<3>() else {
        return Err(unexpected_eof("missing block header"));
    };

    if doc_bits > 32 || tf_bits > 32 || weight_bits > 16 {
        return Err(invalid_data("block bit width out of range"));
    }

    let mut at = 3usize;
    let mut values = [0u32; BLOCK_LEN];

    unpack(packer, bytes, &mut at, doc_bits, &mut values)?;
    let mut doc = entry.first_doc_id;
    for (index, (slot, &gap)) in out.docs.iter_mut().zip(&values).enumerate() {
        if index > 0 {
            doc = doc
                .checked_add(u64::from(gap) + 1)
                .ok_or_else(|| invalid_data("doc id overflow in block"))?;
        }
        *slot = doc;
    }

    unpack(packer, bytes, &mut at, tf_bits, &mut values)?;
    out.term_freq.copy_from_slice(&values);

    if weight_bits == 0 {
        out.weights.fill(entry.max_weight);
    } else {
        unpack(packer, bytes, &mut at, weight_bits, &mut values)?;
        for (weight, &value) in out.weights.iter_mut().zip(&values) {
            *weight = u16::try_from(value).map_err(|e|
                invalid_data(format!("weight does not fit u16: {e}"))
            )?;
        }
    }

    Ok(at)
}

fn unpack(
    packer: BitPacker4x,
    bytes: &[u8],
    at: &mut usize,
    bits: u8,
    values: &mut [u32; BLOCK_LEN]
) -> io::Result<()> {
    if bits == 0 {
        values.fill(0);
        return Ok(());
    }
    let end = at
        .checked_add(packed_len(bits))
        .ok_or_else(|| unexpected_eof("packed block truncated"))?;
    let source = bytes.get(*at..end).ok_or_else(|| unexpected_eof("packed block truncated"))?;
    packer.decompress(source, values, bits);
    *at = end;
    Ok(())
}

pub(crate) fn decode_varint(
    bytes: &[u8],
    entry: &SkipEntry,
    len: usize,
    out: &mut DecodedBlock
) -> io::Result<usize> {
    let has_weights = *bytes.first().ok_or_else(|| unexpected_eof("missing block header"))? != 0;

    let mut at = 1;
    let mut doc = entry.first_doc_id;

    let slots = out.docs
        .iter_mut()
        .zip(out.term_freq.iter_mut())
        .zip(out.weights.iter_mut())
        .take(len);
    for (index, ((doc_slot, freq_slot), weight_slot)) in slots.enumerate() {
        if index > 0 {
            let gap = read_var_u64(bytes, &mut at)?;
            doc = gap
                .checked_add(1)
                .and_then(|step| doc.checked_add(step))
                .ok_or_else(|| invalid_data("doc id overflow in block"))?;
        }
        *doc_slot = doc;
        *freq_slot = read_var_u32(bytes, &mut at)?;
        *weight_slot = if has_weights {
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
            let byte = *input
                .get(*offset)
                .ok_or_else(|| unexpected_eof("unexpected eof while skipping varint"))?;
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
        let [low, ..] = value.to_le_bytes();
        out.push((low & 0x7f) | 0x80);
        value >>= 7;
    }
    let [last, ..] = value.to_le_bytes();
    out.push(last);
}

pub fn push_var_u32(out: &mut Vec<u8>, value: u32) {
    push_var_u64(out, u64::from(value));
}

pub fn push_var_u16(out: &mut Vec<u8>, value: u16) {
    push_var_u64(out, u64::from(value));
}

pub(crate) fn read_var_u64(input: &[u8], offset: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;

    loop {
        let byte = *input
            .get(*offset)
            .ok_or_else(|| unexpected_eof("unexpected eof while reading varint"))?;
        *offset += 1;

        value |= u64::from(byte & 0x7f) << shift;

        if byte & 0x80 == 0 {
            return Ok(value);
        }

        shift += 7;

        if shift >= 64 {
            return Err(invalid_data("varint too large"));
        }
    }
}

pub(crate) fn read_var_u32(input: &[u8], offset: &mut usize) -> io::Result<u32> {
    let value = read_var_u64(input, offset)?;

    u32::try_from(value).map_err(|e| invalid_data(format!("varint does not fit into u32: {e}")))
}

pub(crate) fn read_var_u16(input: &[u8], offset: &mut usize) -> io::Result<u16> {
    let value = read_var_u64(input, offset)?;

    u16::try_from(value).map_err(|e| invalid_data(format!("varint does not fit into u16: {e}")))
}