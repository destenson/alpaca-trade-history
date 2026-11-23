//! OCC option symbol parsing (e.g. ``NVDA261120P00152500``).

use chrono::NaiveDate;

#[derive(Clone, Debug, PartialEq)]
pub struct Contract {
    pub underlying: String,
    pub expiry: NaiveDate,
    pub right: char, // 'C' | 'P'
    pub strike: f64,
}

impl Contract {
    pub fn is_call(&self) -> bool {
        self.right == 'C'
    }
}

pub fn parse_occ(symbol: &str) -> Option<Contract> {
    if symbol.chars().count() < 16 {
        return None;
    }
    let s: String = symbol.chars().filter(|c| *c != ' ').collect();
    if !s.is_ascii() || s.len() < 16 {
        return None;
    }
    let n = s.len();
    let root = &s[..n - 15];
    let tail = &s.as_bytes()[n - 15..];
    if root.is_empty() || root.len() > 6 || !root.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'.') {
        return None;
    }
    if !tail[..6].iter().all(|c| c.is_ascii_digit()) || !tail[7..].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let right = tail[6] as char;
    if right != 'C' && right != 'P' {
        return None;
    }
    let t = std::str::from_utf8(tail).ok()?;
    let yy: i32 = t[0..2].parse().ok()?;
    let mm: u32 = t[2..4].parse().ok()?;
    let dd: u32 = t[4..6].parse().ok()?;
    let expiry = NaiveDate::from_ymd_opt(2000 + yy, mm, dd)?;
    let strike: i64 = t[7..].parse().ok()?;
    Some(Contract { underlying: root.to_string(), expiry, right, strike: strike as f64 / 1000.0 })
}

pub fn underlying_of(symbol: &str) -> String {
    parse_occ(symbol).map(|c| c.underlying).unwrap_or_else(|| symbol.to_string())
}

pub fn is_option(symbol: &str) -> bool {
    parse_occ(symbol).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse() {
        let c = parse_occ("NVDA261120P00152500").unwrap();
        assert_eq!(c.underlying, "NVDA");
        assert_eq!(c.strike, 152.5);
        assert_eq!(c.right, 'P');
        assert!(parse_occ("AAPL").is_none());
    }
}
