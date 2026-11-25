//! Date/time parsing and format handling.
//!
//! A date is normalized to UTC and stored as epoch milliseconds (i64), so it
//! lives in the numeric columns as a plain Int. `format` is either the literal
//! "epoch-millis" (the value is already milliseconds) or a format string built
//! from the tokens YYYY MM DD HH mm ss SSS with literal separators
//! (- / . : T space), optionally ending with a default timezone
//! (Z, +HH:MM, -HH:MM, +HHMM or +HH).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateToken {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Millis,
    Literal(u8),
}
struct MatchedDate {
    date: (i32, u32, u32),
    time: Option<(u32, u32, u32, u32)>,
    /// Bytes of the input the format consumed; anything after is a timezone.
    consumed: usize,
}
/// Parses a date string into epoch milliseconds (UTC), according to `format`.
/// An explicit timezone on the value overrides the format's default.
pub fn parse_date(raw: &str, format: &str) -> Result<i64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty date".to_owned());
    }
    if format == "epoch-millis" {
        return raw
            .parse::<i64>()
            .map_err(|_err| format!("'{raw}' is not epoch milliseconds"));
    }

    let (date_part, default_offset) = strip_default_offset(format);
    let tokens = tokenize_date_format(date_part)?;
    let MatchedDate {
        date: (year, month, day),
        time,
        consumed,
    } = match_date_tokens(raw, &tokens)?;

    let explicit_offset = raw
        .get(consumed..)
        .filter(|rest| !rest.is_empty())
        .map(|rest| {
            parse_tz_offset(rest).map_err(|e| format!("invalid timezone in '{raw}': {e}"))
        })
        .transpose()?;
    let offset_seconds = explicit_offset.or(default_offset).unwrap_or(0);

    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| format!("invalid date '{raw}'"))?;
    let naive = match time {
        Some((h, m, s, ms)) => date
            .and_hms_milli_opt(h, m, s, ms)
            .ok_or_else(|| format!("invalid time in '{raw}'"))?,
        None => date.and_time(chrono::NaiveTime::MIN),
    };
    // naive is the wall-clock time in the given offset; UTC = naive - offset
    Ok(naive.and_utc().timestamp_millis() - offset_seconds * 1000)
}

/// Validates a date format string.
pub fn validate_date_format(format: &str) -> Result<(), String> {
    if format == "epoch-millis" {
        return Ok(());
    }
    let (date_part, _) = strip_default_offset(format);
    tokenize_date_format(date_part)?;
    Ok(())
}

//striping time-zone from the format
fn strip_default_offset(format: &str) -> (&str, Option<i64>) {
    // trailing 'Z' means UTC
    if let Some(head) = format
        .strip_suffix('Z')
        .or_else(|| format.strip_suffix('z'))
    {
        return (head, Some(0));
    }
    // trailing +HH:MM / -HH:MM / +HHMM / -HHMM / +HH / -HH
    if let Some(pos) = format.rfind(['+', '-']) {
        let (head, tz) = format.split_at(pos);
        if !head.is_empty()
            && let Ok(off) = parse_tz_offset(tz)
        {
            return (head, Some(off))
        }
    }
    (format, None)
}

fn parse_tz_offset(s: &str) -> Result<i64, String> {
    if s == "Z" || s == "z" {
        return Ok(0);
    }
    let (sign, rest) = if let Some(rest) = s.strip_prefix('+') {
        (1i64, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (-1i64, rest)
    } else {
        return Err(format!("timezone '{s}' must start with 'Z', '+' or '-'"));
    };
    let bad = || format!("timezone '{s}' must look like Z, +HH:MM, -HH:MM, +HHMM or +HH");
    let (hours, minutes) = match rest.len() {
        5 => {
            let (hh, tail) = rest.split_at_checked(2).ok_or_else(bad)?;
            let mm = tail.strip_prefix(':').ok_or_else(bad)?;
            (parse_2(hh)?, parse_2(mm)?)
        }
        4 => {
            let (hh, mm) = rest.split_at_checked(2).ok_or_else(bad)?;
            (parse_2(hh)?, parse_2(mm)?)
        }
        2 => (parse_2(rest)?, 0),
        _ => return Err(bad()),
    };
    if hours > 14 || minutes > 59 {
        return Err(format!("timezone '{s}' has an invalid offset"));
    }
    Ok(sign * (hours * 3600 + minutes * 60))
}
fn parse_2(s: &str) -> Result<i64, String> {
    if s.len() == 2 && s.bytes().all(|c| c.is_ascii_digit()) {
        s.parse::<i64>().map_err(|_err| format!("bad number '{s}'"))
    } else {
        Err(format!("expected two digits in timezone, got '{s}'"))
    }
}

fn tokenize_date_format(format: &str) -> Result<Vec<DateToken>, String> {
    let mut rest = format.as_bytes();
    let mut tokens = Vec::new();
    while let Some(&first) = rest.first() {
        let (token, len) = if rest.starts_with(b"YYYY") {
            (DateToken::Year, 4)
        } else if rest.starts_with(b"MM") {
            (DateToken::Month, 2)
        } else if rest.starts_with(b"DD") {
            (DateToken::Day, 2)
        } else if rest.starts_with(b"HH") {
            (DateToken::Hour, 2)
        } else if rest.starts_with(b"mm") {
            (DateToken::Minute, 2)
        } else if rest.starts_with(b"ss") {
            (DateToken::Second, 2)
        } else if rest.starts_with(b"SSS") {
            (DateToken::Millis, 3)
        } else {
            if first.is_ascii_alphanumeric() {
                return Err(format!(
                    "unknown token '{}' in date format '{format}' (valid: YYYY MM DD HH mm ss SSS)",
                    char::from(first)
                ));
            }
            (DateToken::Literal(first), 1)
        };
        tokens.push(token);
        rest = rest.get(len..).unwrap_or_default();
    }

    let has = |t: DateToken| tokens.contains(&t);
    if !has(DateToken::Year) || !has(DateToken::Month) || !has(DateToken::Day) {
        return Err(format!(
            "date format '{format}' must contain YYYY, MM and DD"
        ));
    }
    Ok(tokens)
}

fn tail(bytes: &[u8], pos: usize) -> &[u8] {
    bytes.get(pos..).unwrap_or_default()
}

fn match_date_tokens(raw: &str, tokens: &[DateToken]) -> Result<MatchedDate, String> {
    let b = raw.as_bytes();
    let mut pos = 0usize;
    let mut year = None;
    let mut month = None;
    let mut day = None;
    let mut hour = None;
    let mut minute = None;
    let mut second = None;
    let mut millis = None;

    for token in tokens {
        match *token {
            DateToken::Literal(c) => {
                if b.get(pos) != Some(&c) {
                    return Err(format!("expected '{}' in '{raw}'", char::from(c)));
                }
                pos += 1;
            }
            DateToken::Year => {
                let (v, n) = parse_digits(tail(b, pos), 4, 4)
                    .ok_or_else(|| format!("expected 4-digit year in '{raw}'"))?;
                year = Some(
                    i32::try_from(v).map_err(|_overflow| format!("year out of range in '{raw}'"))?,
                );
                pos += n;
            }
            DateToken::Month => {
                let (v, n) = parse_digits(tail(b, pos), 1, 2)
                    .ok_or_else(|| format!("expected month in '{raw}'"))?;
                month = Some(v);
                pos += n;
            }
            DateToken::Day => {
                let (v, n) = parse_digits(tail(b, pos), 1, 2)
                    .ok_or_else(|| format!("expected day in '{raw}'"))?;
                day = Some(v);
                pos += n;
            }
            DateToken::Hour => {
                let (v, n) = parse_digits(tail(b, pos), 1, 2)
                    .ok_or_else(|| format!("expected hour in '{raw}'"))?;
                hour = Some(v);
                pos += n;
            }
            DateToken::Minute => {
                let (v, n) = parse_digits(tail(b, pos), 1, 2)
                    .ok_or_else(|| format!("expected minute in '{raw}'"))?;
                minute = Some(v);
                pos += n;
            }
            DateToken::Second => {
                let (v, n) = parse_digits(tail(b, pos), 1, 2)
                    .ok_or_else(|| format!("expected second in '{raw}'"))?;
                second = Some(v);
                pos += n;
            }
            DateToken::Millis => {
                let (v, n) = parse_digits(tail(b, pos), 1, 3)
                    .ok_or_else(|| format!("expected milliseconds in '{raw}'"))?;
                let ms = match n {
                    1 => v * 100,
                    2 => v * 10,
                    _ => v,
                };
                millis = Some(ms);
                pos += n;
            }
        }
    }

    let year = year.ok_or("date is missing a year")?;
    let month = month.ok_or("date is missing a month")?;
    let day = day.ok_or("date is missing a day")?;

    let time = match (hour, minute, second) {
        (Some(h), Some(m), Some(s)) => Some((h, m, s, millis.unwrap_or(0))),
        (None, None, None) => None,
        _ => return Err("date format has an incomplete time component".to_owned()),
    };

    Ok(MatchedDate {
        date: (year, month, day),
        time,
        consumed: pos,
    })
}
fn parse_digits(s: &[u8], min: usize, max: usize) -> Option<(u32, usize)> {
    let n = s
        .iter()
        .take(max)
        .take_while(|c| c.is_ascii_digit())
        .count();
    let value = s
        .iter()
        .take(n)
        .fold(0u32, |acc, &c| acc * 10 + u32::from(c - b'0'));
    (n >= min).then_some((value, n))
}

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn date_parsing() {
//         // base cases (UTC)
//         assert_eq!(
//             parse_date("2024-01-15", "YYYY-MM-DD").unwrap(),
//             1_705_276_800_000
//         );
//         assert_eq!(
//             parse_date("15-01-2024", "DD-MM-YYYY").unwrap(),
//             1_705_276_800_000
//         );
//         assert_eq!(
//             parse_date("2024-01-15 14:30:00.500", "YYYY-MM-DD HH:mm:ss.SSS").unwrap(),
//             1_705_329_000_500
//         );
//         assert_eq!(
//             parse_date("1705276800000", "epoch-millis").unwrap(),
//             1_705_276_800_000
//         );

//         // $format default timezone: 14:00 +02:00 -> 12:00 UTC
//         assert_eq!(
//             parse_date("2024-01-15 14:00:00", "YYYY-MM-DD HH:mm:ss+02:00").unwrap(),
//             1_705_320_000_000
//         );
//         // explicit value timezone overrides default: 14:00 +03:00 -> 11:00 UTC
//         assert_eq!(
//             parse_date("2024-01-15 14:00:00+03:00", "YYYY-MM-DD HH:mm:ss+02:00").unwrap(),
//             1_705_316_400_000
//         );
//         // Z on the value -> UTC
//         assert_eq!(
//             parse_date("2024-01-15 14:00:00Z", "YYYY-MM-DD HH:mm:ss+02:00").unwrap(),
//             1_705_327_200_000
//         );

//         // invalid dates rejected
//         assert!(parse_date("2024-02-30", "YYYY-MM-DD").is_err());
//         assert!(parse_date("2024-13-01", "YYYY-MM-DD").is_err());
//         assert!(parse_date("nonsense", "epoch-millis").is_err());
//         assert!(parse_date("2024-01-15", "YYYY/MM/DD").is_err());
//     }
// }
