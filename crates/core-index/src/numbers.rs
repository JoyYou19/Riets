//WARN: the next 4 functions work, thats it, they shouldnt be touched by any mortal being
//INFO: used to encode/decode an integer/float to a string that has the same sorting properties
pub fn encode_i64(v: i64) -> String {
    let u = (v as u64) ^ (1u64 << 63);
    format!("{u:016x}")
}

pub fn encode_f64(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v }; // -0.0 == 0.0
    let bits = v.to_bits();
    let u = if bits >> 63 == 1 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    };
    format!("{u:016x}")
}

pub fn decode_i64(term: &str) -> Option<i64> {
    let key = u64::from_str_radix(term, 16).ok()?;
    Some((key ^ (1u64 << 63)) as i64)
}

pub fn decode_f64(term: &str) -> Option<f64> {
    let key = u64::from_str_radix(term, 16).ok()?;
    let bits = if key >> 63 == 1 {
        key ^ (1u64 << 63)
    } else {
        !key
    };
    Some(f64::from_bits(bits))
}

pub fn integer_term(raw: &str) -> Option<String> {
    raw.trim().parse::<i64>().ok().map(encode_i64)
}

pub fn float_term(raw: &str) -> Option<String> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(encode_f64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedBound {
    pub term: String,
    pub inclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NumericRange {
    pub lo: Option<EncodedBound>,
    pub hi: Option<EncodedBound>,
}

fn has_op_prefix(t: &str) -> bool {
    t.starts_with('=') || t.starts_with('>') || t.starts_with('<')
}

//vibe vibe vibe
pub fn parse_filter(raw: &str, encode: fn(&str) -> Option<String>) -> Result<NumericRange, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty numeric filter".to_string());
    }
    if let Some(idx) = s.find("..") {
        let left = s[..idx].trim();
        let right = s[idx + 2..].trim();
        if has_op_prefix(left) || has_op_prefix(right) {
            return Err(format!(
                "comparison operators can't be combined with '..' (use '30..40', or '>40'): '{s}'"
            ));
        }
        if left.is_empty() || right.is_empty() {
            return Err(format!(
                "'..' requires both bounds, e.g. '30..40' (use '>=30' or '<=40' for one-sided ranges): '{s}'"
            ));
        }
        let lo = EncodedBound {
            term: encode(left).ok_or_else(|| format!("invalid lower bound '{left}'"))?,
            inclusive: true,
        };
        let hi = EncodedBound {
            term: encode(right).ok_or_else(|| format!("invalid upper bound '{right}'"))?,
            inclusive: true,
        };
        if lo.term > hi.term {
            return Err(format!(
                "empty range: lower '{left}' is greater than upper '{right}'"
            ));
        }
        return Ok(NumericRange {
            lo: Some(lo),
            hi: Some(hi),
        });
    }

    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = s.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = s.strip_prefix("==") {
        ("==", r)
    } else if let Some(r) = s.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = s.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = s.strip_prefix('=') {
        ("=", r)
    } else {
        ("", s)
    };

    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("missing number after '{op}'"));
    }
    let term = encode(rest).ok_or_else(|| format!("invalid number '{rest}'"))?;

    let range = match op {
        "=" | "==" | "" => {
            let bound = EncodedBound {
                term,
                inclusive: true,
            };
            NumericRange {
                lo: Some(bound.clone()),
                hi: Some(bound),
            }
        }
        ">=" => NumericRange {
            lo: Some(EncodedBound {
                term,
                inclusive: true,
            }),
            hi: None,
        },
        ">" => NumericRange {
            lo: Some(EncodedBound {
                term,
                inclusive: false,
            }),
            hi: None,
        },
        "<=" => NumericRange {
            lo: None,
            hi: Some(EncodedBound {
                term,
                inclusive: true,
            }),
        },
        "<" => NumericRange {
            lo: None,
            hi: Some(EncodedBound {
                term,
                inclusive: false,
            }),
        },
        _ => return Err(format!("unknown operator in '{s}'")),
    };
    Ok(range)
}
