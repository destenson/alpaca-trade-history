//! Estimate option prices after the underlying has moved but the option quotes haven't (see the
//! Python original for the method): reference price -> per-leg IV from the quoted mid -> Black-Scholes
//! at the new price and the valuation time, with time counted in trading days.

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Weekday};
use serde_json::Value;

use crate::jv;
use crate::occ::parse_occ;
use crate::optmath::bs_price;
use crate::util::{self, from_ny, from_ny_naive, ny_date, to_ny, Ts};

pub const R: f64 = 0.04;
pub const TRADING_DAYS: f64 = 252.0;
pub const STALE_MOVE: f64 = 0.02;
pub const WIDE: f64 = 1.0;

pub fn ts(s: &str) -> Option<Ts> {
    util::parse_ts(s)
}

pub fn expiry_time(d: NaiveDate) -> Ts {
    from_ny(d, 16, 0, 0)
}

// ---------------------------------------------------------------------------------- calendar
fn easter(y: i32) -> NaiveDate {
    let (a, b, c) = (y % 19, y / 100, y % 100);
    let (d, e) = (b / 4, b % 4);
    let g = (8 * b + 13) / 25;
    let h = (19 * a + b - d - g + 15) % 30;
    let (i, k) = (c / 4, c % 4);
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    NaiveDate::from_ymd_opt(y, month as u32, ((h + l - 7 * m + 114) % 31 + 1) as u32).unwrap()
}

fn nth_weekday(y: i32, month: u32, wd: u32, n: u32) -> NaiveDate {
    let d = NaiveDate::from_ymd_opt(y, month, 1).unwrap();
    let off = (wd + 7 - d.weekday().num_days_from_monday()) % 7;
    d + Duration::days((off + 7 * (n - 1)) as i64)
}

fn last_weekday(y: i32, month: u32, wd: u32) -> NaiveDate {
    let next = if month == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, month + 1, 1) }.unwrap();
    let d = next - Duration::days(1);
    d - Duration::days(((d.weekday().num_days_from_monday() + 7 - wd) % 7) as i64)
}

fn observed(d: NaiveDate) -> NaiveDate {
    match d.weekday() {
        Weekday::Sat => d - Duration::days(1),
        Weekday::Sun => d + Duration::days(1),
        _ => d,
    }
}

pub fn nyse_holidays(y: i32) -> Vec<NaiveDate> {
    let ymd = |m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
    let mut hs = vec![
        nth_weekday(y, 1, 0, 3),
        nth_weekday(y, 2, 0, 3),
        easter(y) - Duration::days(2),
        last_weekday(y, 5, 0),
        observed(ymd(7, 4)),
        nth_weekday(y, 9, 0, 1),
        nth_weekday(y, 11, 3, 4),
        observed(ymd(12, 25)),
    ];
    if ymd(1, 1).weekday() != Weekday::Sat {
        hs.push(observed(ymd(1, 1)));
    }
    if y >= 2022 {
        hs.push(observed(ymd(6, 19)));
    }
    hs
}

thread_local! {
    static HOL: std::cell::RefCell<HashMap<i32, Vec<NaiveDate>>> = std::cell::RefCell::new(HashMap::new());
}

pub fn is_trading_day(d: NaiveDate) -> bool {
    if d.weekday().num_days_from_monday() >= 5 {
        return false;
    }
    HOL.with(|h| {
        let mut h = h.borrow_mut();
        let v = h.entry(d.year()).or_insert_with(|| nyse_holidays(d.year()));
        !v.contains(&d)
    })
}

/// Trading days between two instants: each non-holiday weekday counts its 24 hours.
pub fn trading_days(t0: &Ts, t1: &Ts) -> f64 {
    if t1 <= t0 {
        return 0.0;
    }
    let (a, b) = (to_ny(t0), to_ny(t1));
    let mut total = 0.0;
    let mut d = a.date();
    while d <= b.date() {
        if is_trading_day(d) {
            let s = a.max(d.and_hms_opt(0, 0, 0).unwrap());
            let e = b.min((d + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap());
            let secs = (e - s).num_microseconds().unwrap_or(0) as f64 / 1e6;
            total += secs.max(0.0) / 86400.0;
        }
        d += Duration::days(1);
    }
    total
}

pub fn years(t0: &Ts, expiry: NaiveDate) -> f64 {
    trading_days(t0, &expiry_time(expiry)).max(60.0 / 86400.0) / TRADING_DAYS
}

pub fn bs(s: f64, k: f64, t: f64, iv: f64, right: char) -> f64 {
    bs_price(s, k, t, iv, right, R)
}

pub fn implied_vol(price: f64, s: f64, k: f64, t: f64, right: char) -> Option<f64> {
    let disc = k * (-R * t).exp();
    let lo_b = if right == 'C' { (s - disc).max(0.0) } else { (disc - s).max(0.0) };
    let hi_b = if right == 'C' { s } else { disc };
    if !(lo_b + 1e-4 < price && price < hi_b) {
        return None;
    }
    let (mut lo, mut hi) = (1e-3, 10.0);
    for _ in 0..100 {
        let m = (lo + hi) / 2.0;
        if bs(s, k, t, m, right) > price {
            hi = m;
        } else {
            lo = m;
        }
    }
    let v = (lo + hi) / 2.0;
    if 1e-3 < v && v < 9.9 {
        Some(v)
    } else {
        None
    }
}

// ------------------------------------------------------------------------------ market state
#[derive(Clone, Debug, Default)]
pub struct Underlying {
    pub symbol: String,
    pub close: Option<f64>,
    pub close_time: Option<Ts>,
    pub last: Option<f64>,
    pub last_time: Option<Ts>,
    pub last_size: Option<f64>,
    pub last_feed: String,
    pub last_ext: bool,
    pub refp: Option<f64>,
    pub spot: Option<f64>,
    pub quote_time: Option<Ts>,
}

impl Underlying {
    pub fn mv(&self) -> f64 {
        match (self.spot, self.refp) {
            (Some(s), Some(r)) if s != 0.0 && r != 0.0 => s / r - 1.0,
            _ => 0.0,
        }
    }
    pub fn stale(&self) -> bool {
        self.mv().abs() >= STALE_MOVE
    }
}

#[derive(Clone, Debug)]
pub struct Market {
    pub is_open: bool,
    pub asof: Ts,
    pub valuation: Ts,
    pub underlyings: BTreeMap<String, Underlying>,
}

pub fn next_open(now: &Ts) -> Ts {
    let d = to_ny(now);
    let mut cand = d.date().and_time(NaiveTime::from_hms_opt(9, 30, 0).unwrap());
    if d >= cand {
        cand += Duration::days(1);
    }
    while !is_trading_day(cand.date()) {
        cand += Duration::days(1);
    }
    from_ny_naive(cand)
}

pub fn market_state(
    clock: Option<&Value>,
    stock_snapshots: Option<&Value>,
    symbols: &[String],
    quote_times: &HashMap<String, Ts>,
    fallback: &HashMap<String, Option<f64>>,
    now: &Ts,
) -> Market {
    let empty = Value::Null;
    let clock = clock.unwrap_or(&empty);
    let now = jv::opt_s(clock, "timestamp").and_then(|s| ts(&s)).unwrap_or(*now);
    let is_open = if clock.get("is_open").is_some() {
        jv::truthy(clock.get("is_open"))
    } else {
        let d = to_ny(&now);
        let t = d.time();
        d.weekday().num_days_from_monday() < 5 && t >= NaiveTime::from_hms_opt(9, 30, 0).unwrap() && t < NaiveTime::from_hms_opt(16, 0, 0).unwrap()
    };
    let valuation = if is_open { now } else { jv::opt_s(clock, "next_open").and_then(|s| ts(&s)).unwrap_or_else(|| next_open(&now)) };
    let snaps = jv::obj(stock_snapshots);
    let mut out = BTreeMap::new();
    let mut syms: Vec<String> = symbols.to_vec();
    syms.sort();
    syms.dedup();
    for sym in syms {
        let mut u = Underlying { symbol: sym.clone(), quote_time: quote_times.get(&sym).copied(), ..Default::default() };
        if let Some(snaps) = snaps {
            let base = snaps.get("default").filter(|v| jv::truthy(Some(v))).or_else(|| snaps.get("")).and_then(|b| b.get(&sym));
            if let Some(base) = base {
                let bar = base.get("dailyBar").unwrap_or(&empty);
                u.close = jv::fpos(bar, "c");
                if u.close.is_some() {
                    if let Some(t) = ts(&jv::s(bar, "t")) {
                        u.close_time = Some(from_ny(ny_date(&t), 16, 0, 0));
                    }
                }
            }
            for (feed, book) in snaps {
                let tr = book.get(&sym).and_then(|s| s.get("latestTrade")).unwrap_or(&empty);
                let t = ts(&jv::s(tr, "t"));
                let p = jv::fpos(tr, "p");
                if let (Some(p), Some(t)) = (p, t) {
                    if u.last_time.map(|lt| t > lt).unwrap_or(true) {
                        let conds: Vec<String> = jv::arr(tr.get("c")).iter().filter_map(|c| c.as_str().map(String::from)).collect();
                        u.last = Some(p);
                        u.last_time = Some(t);
                        u.last_size = jv::fpos(tr, "s");
                        u.last_feed = if feed.is_empty() || feed == "default" { "primary".into() } else { feed.clone() };
                        u.last_ext = feed == "overnight"
                            || conds.iter().any(|c| c == "T" || c == "U")
                            || u.close_time.map(|ct| t > ct + Duration::minutes(1)).unwrap_or(false);
                    }
                }
            }
        }
        let fb = fallback.get(&sym).copied().flatten();
        if is_open {
            u.refp = u.last.or(u.close).or(fb);
            u.spot = u.refp;
        } else {
            u.refp = u.close.or(fb).or(u.last);
            let after = match (u.last, u.close_time, u.last_time) {
                (Some(l), Some(ct), Some(lt)) if lt > ct + Duration::minutes(1) => Some(l),
                _ => None,
            };
            u.spot = after.or(u.refp);
            if u.close_time.is_none() && u.last.is_some() {
                u.spot = u.last;
            }
        }
        out.insert(sym, u);
    }
    Market { is_open, asof: now, valuation, underlyings: out }
}

// --------------------------------------------------------------------------------------- legs
#[derive(Clone, Debug)]
pub struct LegModel {
    pub symbol: String,
    pub strike: f64,
    pub right: char,
    pub expiry: NaiveDate,
    pub iv: f64,
    pub iv_source: &'static str,
    pub t: f64,
}

impl LegModel {
    pub fn value(&self, s: f64, iv_shift: f64) -> f64 {
        bs(s, self.strike, self.t, (self.iv * (1.0 + iv_shift)).max(1e-3), self.right)
    }
}

pub fn calibrate(symbol: &str, snap: &Value, und: &Underlying, market: &Market, fallback_iv: Option<f64>) -> Option<LegModel> {
    let c = parse_occ(symbol)?;
    let empty = Value::Null;
    let q = snap.get("latestQuote").unwrap_or(&empty);
    let (bid, ask) = (jv::fpos(q, "bp"), jv::fpos(q, "ap"));
    let qt = ts(&jv::s(q, "t")).unwrap_or(market.asof);
    let mut iv = None;
    let mut src = "";
    if let (Some(b), Some(a), Some(r)) = (bid, ask, und.refp) {
        let mid = (b + a) / 2.0;
        if (a - b) / mid <= WIDE {
            let t0 = qt.min(expiry_time(c.expiry));
            iv = implied_vol(mid, r, c.strike, years(&t0, c.expiry), c.right);
            if iv.is_some() {
                src = "quote mid";
            }
        }
    }
    if iv.is_none() {
        if let Some(v) = jv::fpos(snap, "impliedVolatility") {
            iv = Some(v);
            src = "Alpaca IV";
        }
    }
    if iv.is_none() {
        if let Some(v) = fallback_iv.filter(|v| *v != 0.0) {
            iv = Some(v);
            src = "underlying avg";
        }
    }
    let iv = iv.unwrap_or_else(|| {
        src = "default";
        0.5
    });
    Some(LegModel { symbol: symbol.to_string(), strike: c.strike, right: c.right, expiry: c.expiry, iv, iv_source: src, t: years(&market.valuation, c.expiry) })
}

pub fn underlying_ivs(snapshots: &Value) -> HashMap<String, f64> {
    let mut acc: HashMap<String, Vec<f64>> = HashMap::new();
    if let Some(o) = jv::obj(Some(snapshots)) {
        for (sym, s) in o {
            if let (Some(c), Some(iv)) = (parse_occ(sym), jv::fpos(s, "impliedVolatility")) {
                acc.entry(c.underlying).or_default().push(iv);
            }
        }
    }
    acc.into_iter().map(|(k, v)| (k, v.iter().sum::<f64>() / v.len() as f64)).collect()
}

pub fn quote_times(snapshots: &Value) -> HashMap<String, Ts> {
    let mut out: HashMap<String, Ts> = HashMap::new();
    if let Some(o) = jv::obj(Some(snapshots)) {
        for (sym, s) in o {
            let t = s.get("latestQuote").and_then(|q| ts(&jv::s(q, "t")));
            if let (Some(c), Some(t)) = (parse_occ(sym), t) {
                let e = out.entry(c.underlying).or_insert(t);
                if t > *e {
                    *e = t;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar() {
        assert!(!is_trading_day(util::date_ymd(2026, 11, 26))); // Thanksgiving
        assert!(!is_trading_day(util::date_ymd(2026, 4, 3))); // Good Friday
        assert!(is_trading_day(util::date_ymd(2026, 11, 23)));
        let fri = from_ny(util::date_ymd(2026, 11, 20), 16, 0, 0);
        let mon = from_ny(util::date_ymd(2026, 11, 23), 9, 30, 0);
        let td = trading_days(&fri, &mon);
        assert!((td - (8.0 / 24.0 + 9.5 / 24.0)).abs() < 1e-9, "{}", td);
    }
}
