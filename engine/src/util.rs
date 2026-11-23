//! Small numeric, formatting and time helpers that mirror the Python/pandas behaviour the report was
//! written against (number formats, rounding, skip-NaN statistics, New York local time).

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc, Weekday};

pub type Ts = DateTime<Utc>;
pub const NAN: f64 = f64::NAN;

// ------------------------------------------------------------------------------------------ numbers
pub fn finite(x: f64) -> bool {
    x.is_finite()
}

/// numpy-style rounding to ``d`` decimals (round half to even on the scaled value).
pub fn round_dp(x: f64, d: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let p = 10f64.powi(d);
    let y = (x * p).round_ties_even() / p;
    if y == 0.0 {
        0.0
    } else {
        y
    }
}

pub fn round_vec(v: &[f64], d: i32) -> Vec<f64> {
    v.iter().map(|x| round_dp(*x, d)).collect()
}

fn valid(v: &[f64]) -> impl Iterator<Item = f64> + '_ {
    v.iter().copied().filter(|x| !x.is_nan())
}

pub fn sum(v: &[f64]) -> f64 {
    valid(v).sum()
}

pub fn mean(v: &[f64]) -> f64 {
    let n = valid(v).count();
    if n == 0 {
        return NAN;
    }
    sum(v) / n as f64
}

pub fn median(v: &[f64]) -> f64 {
    let mut s: Vec<f64> = valid(v).collect();
    if s.is_empty() {
        return NAN;
    }
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Sample standard deviation (ddof = 1), NaNs skipped; NaN with fewer than two values.
pub fn std1(v: &[f64]) -> f64 {
    let s: Vec<f64> = valid(v).collect();
    let n = s.len();
    if n < 2 {
        return NAN;
    }
    let m = s.iter().sum::<f64>() / n as f64;
    let ss: f64 = s.iter().map(|x| (x - m) * (x - m)).sum();
    (ss / (n as f64 - 1.0)).sqrt()
}

pub fn var1(v: &[f64]) -> f64 {
    let s = std1(v);
    s * s
}

pub fn cov1(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n < 2 {
        return NAN;
    }
    let ma = a[..n].iter().sum::<f64>() / n as f64;
    let mb = b[..n].iter().sum::<f64>() / n as f64;
    let s: f64 = (0..n).map(|i| (a[i] - ma) * (b[i] - mb)).sum();
    s / (n as f64 - 1.0)
}

pub fn vmax(v: &[f64]) -> f64 {
    valid(v).fold(NAN, |m, x| if m.is_nan() || x > m { x } else { m })
}

pub fn vmin(v: &[f64]) -> f64 {
    valid(v).fold(NAN, |m, x| if m.is_nan() || x < m { x } else { m })
}

pub fn cumsum(v: &[f64]) -> Vec<f64> {
    let mut acc = 0.0;
    v.iter()
        .map(|x| {
            acc += x;
            acc
        })
        .collect()
}

pub fn cummax(v: &[f64]) -> Vec<f64> {
    let mut m = NAN;
    v.iter()
        .map(|&x| {
            if x.is_nan() {
                return NAN;
            }
            if m.is_nan() || x > m {
                m = x;
            }
            m
        })
        .collect()
}

/// numpy.linspace (the last point is exactly ``b``).
pub fn linspace(a: f64, b: f64, n: usize) -> Vec<f64> {
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![a];
    }
    let step = (b - a) / (n as f64 - 1.0);
    let mut out: Vec<f64> = (0..n).map(|i| a + i as f64 * step).collect();
    out[n - 1] = b;
    out
}

/// numpy.interp: linear interpolation, clamped at the ends.
pub fn interp(x: f64, xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len();
    if n == 0 {
        return NAN;
    }
    if x <= xs[0] {
        return ys[0];
    }
    if x >= xs[n - 1] {
        return ys[n - 1];
    }
    let mut lo = 0;
    let mut hi = n - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if xs[mid] <= x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let t = (x - xs[lo]) / (xs[hi] - xs[lo]);
    ys[lo] + t * (ys[hi] - ys[lo])
}

pub fn erf(x: f64) -> f64 {
    libm::erf(x)
}

// --------------------------------------------------------------------------------------- formatting
/// Python ``f"{x:,.{dp}f}"`` (thousands separators; a leading '-' for negatives).
pub fn commas(x: f64, dp: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format!("{:.*}", dp, x.abs());
    let (int, frac) = match s.find('.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (&s[..], ""),
    };
    let mut out = String::new();
    let b = int.as_bytes();
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*c as char);
    }
    format!("{}{}{}", if x.is_sign_negative() { "-" } else { "" }, out, frac)
}

pub fn money(x: f64, signed: bool, dp: usize) -> String {
    if x.is_nan() {
        return "–".into();
    }
    if x.is_infinite() {
        return format!("{}$inf", if x < 0.0 { "−" } else if signed { "+" } else { "" });
    }
    let s = commas(x.abs(), dp);
    let sign = if x < 0.0 { "−" } else if signed && x > 0.0 { "+" } else { "" };
    format!("{}${}", sign, s)
}

pub fn money2(x: f64) -> String {
    money(x, false, 2)
}

pub fn pct(x: f64, signed: bool, dp: usize) -> String {
    if x.is_nan() {
        return "–".into();
    }
    let sign = if x < 0.0 { "−" } else if signed && x > 0.0 { "+" } else { "" };
    format!("{}{}%", sign, commas((x * 100.0).abs(), dp))
}

pub fn num(x: f64, dp: usize) -> String {
    if x.is_nan() {
        return "–".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "−inf".into() };
    }
    commas(x, dp).replace('-', "−")
}

/// Python ``f"{x:.{dp}%}"`` / ``f"{x:+.{dp}%}"`` (ASCII '-').
pub fn pyfmt_pct(x: f64, dp: usize, plus: bool) -> String {
    if x.is_nan() {
        return "nan%".into();
    }
    // Python keeps the sign of the value even when it rounds to zero ('-0%')
    let body = format!("{:.*}", dp, (x * 100.0).abs());
    let sign = if x.is_sign_negative() { "-" } else if plus { "+" } else { "" };
    format!("{}{}%", sign, body)
}

/// Python ``f"{x:g}"``.
pub fn g(x: f64) -> String {
    fmt_g(x, 6)
}

pub fn fmt_g(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let p = prec.max(1);
    let e = format!("{:.*e}", p - 1, x);
    let (mant, exp) = e.split_at(e.find('e').unwrap());
    let exp: i32 = exp[1..].parse().unwrap();
    if exp >= -4 && exp < p as i32 {
        let dec = (p as i32 - 1 - exp).max(0) as usize;
        let s = format!("{:.*}", dec, x);
        strip_zeros(&s)
    } else {
        let m = strip_zeros(mant);
        format!("{}e{}{:02}", m, if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        let t = s.trim_end_matches('0');
        t.trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

/// Python ``repr(float)`` / ``str(float)``.
pub fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let e = format!("{:e}", x); // shortest round-trip digits
    let (mant, exp) = e.split_at(e.find('e').unwrap());
    let exp: i32 = exp[1..].parse().unwrap();
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let n = digits.len() as i32;
        if exp >= 0 {
            if n <= exp + 1 {
                format!("{}{}{}.0", sign, digits, "0".repeat((exp + 1 - n) as usize))
            } else {
                let (a, b) = digits.split_at((exp + 1) as usize);
                format!("{}{}.{}", sign, a, b)
            }
        } else {
            format!("{}0.{}{}", sign, "0".repeat((-exp - 1) as usize), digits)
        }
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        format!("{}{}e{}{:02}", sign, m, if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// html.escape(s, quote=True)
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#x27;"),
            _ => o.push(c),
        }
    }
    o
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#x27;", "'").replace("&amp;", "&")
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { one.to_string() } else { many.to_string() }
}

// ----------------------------------------------------------------------------------- New York time
/// US daylight saving (rules in force since 2007): second Sunday of March 02:00 to first Sunday of
/// November 02:00, local time.
fn nth_sunday(y: i32, m: u32, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let off = (7 - first.weekday().num_days_from_sunday()) % 7;
    first + Duration::days((off + 7 * (n - 1)) as i64)
}

fn dst_bounds_utc(y: i32) -> (NaiveDateTime, NaiveDateTime) {
    let start = nth_sunday(y, 3, 2).and_hms_opt(7, 0, 0).unwrap(); // 02:00 EST = 07:00 UTC
    let end = nth_sunday(y, 11, 1).and_hms_opt(6, 0, 0).unwrap(); // 02:00 EDT = 06:00 UTC
    (start, end)
}

pub fn ny_offset_hours(t: &Ts) -> i64 {
    let n = t.naive_utc();
    let (a, b) = dst_bounds_utc(n.year());
    if n >= a && n < b {
        -4
    } else {
        -5
    }
}

pub fn to_ny(t: &Ts) -> NaiveDateTime {
    t.naive_utc() + Duration::hours(ny_offset_hours(t))
}

pub fn ny_date(t: &Ts) -> NaiveDate {
    to_ny(t).date()
}

/// A New York wall-clock time as an instant (ambiguous times take the daylight offset, like zoneinfo).
pub fn from_ny_naive(l: NaiveDateTime) -> Ts {
    let y = l.year();
    let ds = nth_sunday(y, 3, 2).and_hms_opt(2, 0, 0).unwrap();
    let de = nth_sunday(y, 11, 1).and_hms_opt(1, 0, 0).unwrap(); // 01:00-02:00 is ambiguous: fold=0 -> DST
    let off = if l >= ds && l < de + Duration::hours(1) { 4 } else { 5 };
    Utc.from_utc_datetime(&(l + Duration::hours(off)))
}

pub fn from_ny(d: NaiveDate, h: u32, m: u32, s: u32) -> Ts {
    from_ny_naive(d.and_hms_opt(h, m, s).unwrap())
}

pub fn date_ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// Parse an ISO-8601 timestamp (date only, naive = UTC, Z or ±hh:mm offsets, any fraction length).
pub fn parse_ts(s: &str) -> Option<Ts> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.len() == 10 {
        let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    let b = s.as_bytes();
    if b.len() < 16 {
        return None;
    }
    let d = NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?;
    let rest = &s[11..];
    // split time and offset
    let mut end = rest.len();
    let mut off_min: i64 = 0;
    if let Some(stripped) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        end = stripped.len();
    } else if let Some(p) = rest.rfind(|c| c == '+' || c == '-') {
        if p >= 5 {
            let o = &rest[p..];
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let digits: String = o[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 2 {
                let hh: i64 = digits[..2].parse().ok()?;
                let mm: i64 = if digits.len() >= 4 { digits[2..4].parse().ok()? } else { 0 };
                off_min = sign * (hh * 60 + mm);
                end = p;
            }
        }
    }
    let tpart = &rest[..end];
    let (hms, frac) = match tpart.find('.') {
        Some(i) => (&tpart[..i], &tpart[i + 1..]),
        None => (tpart, ""),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let h: u32 = parts.first()?.parse().ok()?;
    let mi: u32 = parts.get(1).unwrap_or(&"0").parse().ok()?;
    let se: u32 = parts.get(2).unwrap_or(&"0").parse().ok()?;
    let mut micros: u32 = 0;
    if !frac.is_empty() {
        let f: String = frac.chars().take_while(|c| c.is_ascii_digit()).take(6).collect();
        let padded = format!("{:0<6}", f);
        micros = padded.parse().ok()?;
    }
    let t = NaiveTime::from_hms_micro_opt(h, mi, se, micros)?;
    let ndt = d.and_time(t) - Duration::minutes(off_min);
    Some(Utc.from_utc_datetime(&ndt))
}

pub fn from_unix(secs: f64) -> Ts {
    Utc.timestamp_opt(secs.floor() as i64, 0).unwrap()
}

pub fn days_between(a: NaiveDate, b: NaiveDate) -> i64 {
    (b - a).num_days()
}

pub fn secs(a: &Ts, b: &Ts) -> f64 {
    (*b - *a).num_microseconds().map(|u| u as f64 / 1e6).unwrap_or(0.0)
}

pub fn iso_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// report._iso for a timestamp: New York date, plus HH:MM unless it is midnight.
pub fn iso_ts(t: &Ts) -> String {
    let l = to_ny(t);
    if l.hour() == 0 && l.minute() == 0 && l.second() == 0 {
        l.format("%Y-%m-%d").to_string()
    } else {
        l.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Midnight New York of the timestamp's New York date (pandas ``.dt.normalize()`` on NY times).
pub fn ny_midnight(t: &Ts) -> Ts {
    from_ny(ny_date(t), 0, 0, 0)
}

/// '9:30 AM'
pub fn clock(l: &NaiveDateTime) -> String {
    let h = l.hour();
    format!("{}:{:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, l.minute(), if h < 12 { "AM" } else { "PM" })
}

pub fn is_weekend(d: NaiveDate) -> bool {
    matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

pub fn fmt_d(d: NaiveDate, f: &str) -> String {
    d.format(f).to_string()
}

pub fn sha1_hex(s: &str) -> String {
    sha1_smol::Sha1::from(s.as_bytes()).digest().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(g(1.0), "1");
        assert_eq!(g(-2.5), "-2.5");
        assert_eq!(g(152.5), "152.5");
        assert_eq!(g(1234567.0), "1.23457e+06");
        assert_eq!(g(0.0001), "0.0001");
        assert_eq!(g(0.00001), "1e-05");
        assert_eq!(py_float(100.0), "100.0");
        assert_eq!(py_float(0.1), "0.1");
        assert_eq!(py_float(1e-5), "1e-05");
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(-252.41), "-252.41");
        assert_eq!(py_float(123456789012345.6), "123456789012345.6");
        assert_eq!(money(1234.5, true, 2), "+$1,234.50");
        assert_eq!(money(-0.4, false, 0), "−$0");
        assert_eq!(pct(-0.1234, false, 1), "−12.3%");
        assert_eq!(num(-1234.567, 2), "−1,234.57");
        assert_eq!(pyfmt_pct(0.123, 1, true), "+12.3%");
        assert_eq!(pyfmt_pct(-0.123, 0, false), "-12%");
        assert_eq!(commas(999.999, 2), "1,000.00");
    }

    #[test]
    fn ny_time() {
        let t = parse_ts("2026-11-20T21:00:00Z").unwrap();
        assert_eq!(to_ny(&t).to_string(), "2026-11-20 16:00:00");
        let t = parse_ts("2026-07-01T20:00:00Z").unwrap();
        assert_eq!(to_ny(&t).to_string(), "2026-07-01 16:00:00");
        assert_eq!(from_ny(date_ymd(2026, 7, 1), 16, 0, 0), t);
        let t = parse_ts("2026-11-20T16:00:00.123456789-05:00").unwrap();
        assert_eq!(to_ny(&t).to_string(), "2026-11-20 16:00:00.123456");
    }
}
