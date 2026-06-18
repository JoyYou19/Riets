use std::io::{self};
/*
* Supposed to encode/decode more compact varints, but with my extensive testing
* the performance gain might be negligible
*/

// Bitwise operations incoming broom broom, but in reality unsigned LEB128 style variable length
// integer codec
pub fn push_var_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }

    out.push(value as u8);
}

pub fn push_var_u32(out: &mut Vec<u8>, value: u32) {
    push_var_u64(out, value as u64);
}

pub fn push_var_u16(out: &mut Vec<u8>, value: u16) {
    push_var_u64(out, value as u64);
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

        value |= ((byte & 0x7f) as u64) << shift;

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
