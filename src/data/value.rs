//! Reading cell text as numbers, and guessing what a column holds.

use super::Data;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ColType {
    #[default]
    Text,
    Number,
    Date,
    Bool,
}

impl ColType {
    pub fn badge(self) -> &'static str {
        match self {
            ColType::Text => "Aa",
            ColType::Number => "#",
            ColType::Date => "D",
            ColType::Bool => "✓",
        }
    }
    pub fn numeric(self) -> bool {
        self == ColType::Number
    }
}

#[inline]
fn trim(b: &[u8]) -> &[u8] {
    b.trim_ascii()
}

/// A cell as a number: plain numbers, `1,234.5`, `$12`, `-€3.50`, `45%`,
/// `(12)` for negatives. Returns `None` for anything else.
pub fn parse_number(raw: &[u8]) -> Option<f64> {
    let mut b = trim(raw);
    if b.is_empty() || b.len() > 64 {
        return None;
    }
    if let Some(v) = parse_plain(b) {
        return Some(v);
    }
    // Plain digits the quick parser can't take exactly (long, exponents).
    if let Ok(s) = std::str::from_utf8(b)
        && let Some(c) = s.as_bytes().first()
        && (c.is_ascii_digit() || *c == b'-' || *c == b'.' || *c == b'+')
        && let Ok(v) = s.parse::<f64>()
        && (v.is_finite() && !s.contains(['e', 'E', 'i', 'n', 'I', 'N']) || is_sci(s))
    {
        return Some(v);
    }
    let mut neg = false;
    if b.first() == Some(&b'(') && b.last() == Some(&b')') {
        neg = true;
        b = trim(&b[1..b.len() - 1]);
    }
    if let Some(r) = b.strip_prefix(b"-") {
        neg = !neg;
        b = trim(r);
    }
    let mut pct = false;
    if let Some(r) = b.strip_suffix(b"%") {
        pct = true;
        b = trim(r);
    }
    for sym in ["$", "€", "£", "¥"] {
        if let Some(r) = b.strip_prefix(sym.as_bytes()) {
            b = trim(r);
            break;
        }
    }
    if let Some(r) = b.strip_prefix(b"-") {
        neg = !neg;
        b = trim(r);
    }
    if b.is_empty() || !b[0].is_ascii_digit() && b[0] != b'.' {
        return None;
    }
    let mut s = String::with_capacity(b.len());
    let mut seen_dot = false;
    let mut group = 0usize;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'0'..=b'9' => {
                s.push(c as char);
                group += 1;
            }
            b',' if !seen_dot => {
                // Thousands separator: exactly three digits must follow.
                if i == 0 || group > 3 && s.len() != group {
                    return None;
                }
                let rest = &b[i + 1..];
                if rest.len() < 3 || !rest[..3].iter().all(u8::is_ascii_digit) {
                    return None;
                }
                if rest.len() > 3 && rest[3].is_ascii_digit() {
                    return None;
                }
                group = 0;
            }
            b'.' if !seen_dot => {
                seen_dot = true;
                s.push('.');
            }
            _ => return None,
        }
    }
    let v: f64 = s.parse().ok()?;
    let v = if pct { v / 100.0 } else { v };
    Some(if neg { -v } else { v })
}

/// Powers of ten that are exact in an f64.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// `[+-]digits[.digits]` with at most 15 significant digits, the common case
/// in data files. With the mantissa below 2^53 and an exact power of ten, one
/// division is correctly rounded (Clinger's fast path), so this gives the
/// same result as `str::parse`. Anything else returns `None`.
#[inline]
fn parse_plain(b: &[u8]) -> Option<f64> {
    let (neg, digits) = match b[0] {
        b'-' => (true, &b[1..]),
        b'+' => (false, &b[1..]),
        _ => (false, b),
    };
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    let mut m: u64 = 0;
    let mut frac: Option<usize> = None;
    let mut nd = 0usize;
    for (i, &c) in digits.iter().enumerate() {
        match c {
            b'0'..=b'9' => {
                m = m * 10 + (c - b'0') as u64;
                nd += 1;
            }
            b'.' if frac.is_none() => frac = Some(i + 1),
            _ => return None,
        }
    }
    if nd == 0 || nd > 15 {
        return None;
    }
    let scale = frac.map_or(0, |f| digits.len() - f);
    let v = m as f64 / POW10[scale];
    Some(if neg { -v } else { v })
}

fn is_sci(s: &str) -> bool {
    let b = s.as_bytes();
    let Some(e) = b.iter().position(|&c| c == b'e' || c == b'E') else {
        return false;
    };
    e > 0
        && b[e - 1].is_ascii_digit()
        && b[e + 1..]
            .iter()
            .enumerate()
            .all(|(i, &c)| c.is_ascii_digit() || i == 0 && (c == b'-' || c == b'+'))
        && e + 1 < b.len()
}

/// `YYYY-MM-DD`, optionally followed by a time, or `MM/DD/YYYY`.
pub fn looks_like_date(raw: &[u8]) -> bool {
    let b = trim(raw);
    let iso = b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit);
    if iso {
        return true;
    }
    let parts: Vec<&[u8]> = b.split(|&c| c == b'/').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.iter().all(u8::is_ascii_digit))
        && parts[2].len() == 4
}

fn is_bool(raw: &[u8]) -> bool {
    let b = trim(raw);
    b.eq_ignore_ascii_case(b"true") || b.eq_ignore_ascii_case(b"false")
}

/// Guesses column types from up to `sample` rows spread over the data.
pub fn infer_types(data: &Data, first_row: u64, ncols: usize, sample: usize) -> Vec<ColType> {
    let rows = data.rows.saturating_sub(first_row);
    if rows == 0 || ncols == 0 {
        return vec![ColType::Text; ncols];
    }
    // The first rows plus an even spread over the rest.
    let mut picks: Vec<u64> = (0..rows.min(sample as u64 / 2)).collect();
    let step = (rows / (sample as u64 / 2).max(1)).max(1);
    picks.extend((0..rows).step_by(step as usize).take(sample / 2));
    picks.sort_unstable();
    picks.dedup();

    let mut counts = vec![[0u32; 4]; ncols]; // non-empty, number, date, bool
    for &r in &picks {
        data.for_each_field(first_row + r, ncols, |c, v| {
            let v = trim(&v);
            if v.is_empty() {
                return;
            }
            let k = &mut counts[c];
            k[0] += 1;
            if parse_number(v).is_some() {
                k[1] += 1;
            } else if looks_like_date(v) {
                k[2] += 1;
            } else if is_bool(v) {
                k[3] += 1;
            }
        });
    }
    counts
        .iter()
        .map(|k| {
            let n = k[0].max(1) as f32;
            if k[0] == 0 {
                ColType::Text
            } else if k[1] as f32 / n >= 0.9 {
                ColType::Number
            } else if k[2] as f32 / n >= 0.9 {
                ColType::Date
            } else if k[3] as f32 / n >= 0.95 {
                ColType::Bool
            } else {
                ColType::Text
            }
        })
        .collect()
}

/// Formats a number the way a spreadsheet's General format does: integers
/// without a fraction, others with up to 10 significant decimals.
pub fn format_number(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    if v.abs() >= 1e15 || v.abs() < 1e-9 && v != 0.0 {
        return format!("{v:e}");
    }
    let s = format!("{v:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// Formats a number with thousands separators and `decimals` places.
pub fn format_grouped(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    let s = format!("{:.*}", decimals, v.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut out = String::with_capacity(s.len() + 8);
    if v < 0.0 && s.bytes().any(|c| c != b'0' && c != b'.') {
        out.push('-');
    }
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// A count with thousands separators.
pub fn count(n: u64) -> String {
    format_grouped(n as f64, 0)
}

/// An Excel serial date (days since 1899-12-30) as `YYYY-MM-DD[ HH:MM[:SS]]`.
pub fn excel_date(serial: f64, is_1904: bool) -> String {
    let serial = if is_1904 { serial + 1462.0 } else { serial };
    if !(0.0..2_958_466.0).contains(&serial) {
        return format_number(serial);
    }
    let days = serial.floor() as i64;
    let mut secs = ((serial - serial.floor()) * 86_400.0).round() as i64;
    let mut days = days;
    if secs >= 86_400 {
        secs -= 86_400;
        days += 1;
    }
    // Days since 1899-12-30 to a civil date (Howard Hinnant's algorithm).
    let z = days - 25_569 + 719_468; // 1970-01-01 is serial 25569
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if secs == 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else if s == 0 {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    }
}

/// A column's letters: 0 → A, 25 → Z, 26 → AA.
pub fn col_name(mut c: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

/// Parses column letters back to an index.
pub fn parse_col_name(s: &str) -> Option<usize> {
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut n = 0usize;
    for c in s.bytes() {
        n = n * 26 + (c.to_ascii_uppercase() - b'A') as usize + 1;
    }
    Some(n - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_numbers_match_std() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let digits = 1 + (x % 15) as usize;
            let mut s = String::new();
            if x & (1 << 40) != 0 {
                s.push('-');
            }
            let mut y = x >> 8;
            for _ in 0..digits {
                s.push((b'0' + (y % 10) as u8) as char);
                y /= 10;
            }
            let dot = (x >> 50) as usize % (digits + 2);
            if dot <= digits {
                s.insert(s.len() - digits + dot, '.');
            }
            let want: f64 = s.parse().unwrap();
            let got = parse_plain(s.as_bytes()).unwrap();
            assert_eq!(got.to_bits(), want.to_bits(), "{s}");
        }
        assert_eq!(parse_plain(b"1234567890123456"), None);
        assert_eq!(parse_plain(b"."), None);
        assert_eq!(parse_plain(b"1.2.3"), None);
        assert_eq!(parse_number(b"1234567890123456"), Some(1234567890123456.0));
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number(b"42"), Some(42.0));
        assert_eq!(parse_number(b" -3.5 "), Some(-3.5));
        assert_eq!(parse_number(b"1,234.5"), Some(1234.5));
        assert_eq!(parse_number(b"$1,000"), Some(1000.0));
        assert_eq!(parse_number(b"-$12"), Some(-12.0));
        assert_eq!(parse_number(b"(12)"), Some(-12.0));
        assert_eq!(parse_number(b"45%"), Some(0.45));
        assert_eq!(parse_number(b"1e3"), Some(1000.0));
        assert_eq!(parse_number(b".5"), Some(0.5));
        assert_eq!(parse_number(b"1,23"), None);
        assert_eq!(parse_number(b"abc"), None);
        assert_eq!(parse_number(b"inf"), None);
        assert_eq!(parse_number(b"NaN"), None);
        assert_eq!(parse_number(b"12abc"), None);
        assert_eq!(parse_number(b""), None);
        assert_eq!(parse_number(b"2024-01-01"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(excel_date(45943.5, false), "2025-10-13 12:00");
        assert_eq!(excel_date(1.0, false), "1899-12-31");
        assert_eq!(excel_date(25569.0, false), "1970-01-01");
        assert!(looks_like_date(b"2024-03-01"));
        assert!(looks_like_date(b"3/1/2024"));
        assert!(!looks_like_date(b"hello"));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(format_grouped(1234567.891, 2), "1,234,567.89");
        assert_eq!(format_grouped(-1000.0, 0), "-1,000");
        assert_eq!(count(999), "999");
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(701), "ZZ");
        assert_eq!(col_name(702), "AAA");
        assert_eq!(parse_col_name("AA"), Some(26));
        assert_eq!(parse_col_name("b"), Some(1));
    }
}
