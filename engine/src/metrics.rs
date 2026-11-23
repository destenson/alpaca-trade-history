//! Account-level performance: equity curve, cash-flow-adjusted returns, drawdowns, risk ratios and
//! benchmark comparison.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use serde_json::Value;

use crate::jv;
use crate::ledger::{Kind, Ledger};
use crate::util::{self, cummax, cumsum, mean, ny_date, std1, NAN};

pub const TRADING_DAYS: f64 = 252.0;

#[derive(Clone, Debug, Default)]
pub struct EqFrame {
    pub dates: Vec<NaiveDate>,
    pub equity: Vec<f64>,
    pub flow: Vec<f64>,
    pub pnl: Vec<f64>,
    pub ret: Vec<f64>,
    pub twr: Vec<f64>,
    pub net_deposits: Vec<f64>,
    pub cum_pnl: Vec<f64>,
    pub dd: Vec<f64>,
    pub dd_dollar: Vec<f64>,
    pub dropped: Vec<(NaiveDate, f64)>,
    pub funded: bool,
}

impl EqFrame {
    pub fn len(&self) -> usize {
        self.dates.len()
    }
    pub fn is_empty(&self) -> bool {
        self.dates.is_empty()
    }
    pub fn slice_from(&self, i: usize) -> EqFrame {
        let s = |v: &Vec<f64>| if v.len() == self.dates.len() { v[i..].to_vec() } else { v.clone() };
        EqFrame {
            dates: self.dates[i..].to_vec(),
            equity: s(&self.equity),
            flow: s(&self.flow),
            pnl: s(&self.pnl),
            ret: s(&self.ret),
            twr: s(&self.twr),
            net_deposits: s(&self.net_deposits),
            cum_pnl: s(&self.cum_pnl),
            dd: s(&self.dd),
            dd_dollar: s(&self.dd_dollar),
            dropped: self.dropped.clone(),
            funded: self.funded,
        }
    }
}

pub type Closes = Vec<(NaiveDate, f64)>;

/// Last close on or before ``d``.
pub fn last_le(c: &Closes, d: NaiveDate) -> Option<f64> {
    let i = c.partition_point(|(x, _)| *x <= d);
    if i == 0 {
        None
    } else {
        Some(c[i - 1].1)
    }
}

pub fn bar_closes(bars: &Value, symbol: &str) -> Option<Closes> {
    let rows = jv::arr(bars.get(symbol));
    if rows.is_empty() {
        return None;
    }
    let mut m: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for b in rows {
        let Some(t) = util::parse_ts(&jv::s(b, "t")) else { continue };
        m.insert(ny_date(&t), jv::f(b, "c").unwrap_or(NAN));
    }
    Some(m.into_iter().collect())
}

pub fn equity_series(ph: &Value, ledger: &Ledger) -> Result<EqFrame, String> {
    let ts = jv::arr(ph.get("timestamp"));
    if ts.is_empty() {
        return Err("portfolio_history with daily timestamps is required for account-level metrics".into());
    }
    let eqv = jv::arr(ph.get("equity"));
    let raw: Vec<(NaiveDate, f64)> = ts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let d = ny_date(&util::from_unix(jv::float_of(t).unwrap_or(0.0)));
            (d, eqv.get(i).and_then(jv::float_of).unwrap_or(NAN))
        })
        .collect();
    // drop duplicated dates, keeping the last
    let mut keep: Vec<(NaiveDate, f64)> = Vec::new();
    for (i, (d, v)) in raw.iter().enumerate() {
        if raw[i + 1..].iter().any(|(x, _)| x == d) {
            continue;
        }
        keep.push((*d, *v));
    }
    let dates: Vec<NaiveDate> = keep.iter().map(|x| x.0).collect();
    let mut equity: Vec<f64> = keep.iter().map(|x| x.1).collect();
    let mut flow = vec![0.0; dates.len()];
    let mut dropped = Vec::new();
    let cash = ledger.cash_rows();
    let flows: Vec<_> = cash.iter().filter(|c| c.kind == Kind::Flow).collect();
    if !flows.is_empty() {
        let mut by: BTreeMap<NaiveDate, f64> = BTreeMap::new();
        for c in flows {
            *by.entry(c.date).or_insert(0.0) += c.cash;
        }
        for (d, v) in by {
            if d < dates[0] {
                continue;
            }
            match dates.iter().position(|x| *x >= d) {
                Some(i) => flow[i] += v,
                None => dropped.push((d, v)),
            }
        }
    } else if let Some(cf) = jv::obj(ph.get("cashflow")) {
        for k in ["CSD", "CSW", "JNLC", "ACATC"] {
            if let Some(Value::Array(a)) = cf.get(k) {
                for (i, f) in flow.iter_mut().enumerate() {
                    let v = a.get(i).and_then(jv::float_of).unwrap_or(0.0);
                    *f += if v.is_nan() { 0.0 } else { v };
                }
            }
        }
    }
    // ffill then 0
    let mut last = NAN;
    for e in equity.iter_mut() {
        if e.is_nan() {
            *e = last;
        } else {
            last = *e;
        }
        if e.is_nan() {
            *e = 0.0;
        }
    }
    let mut f = EqFrame { dates, equity, flow, dropped, ..Default::default() };
    // start at the most recent funded stretch
    if let Some(lu) = f.dates.iter().zip(&f.equity).filter(|(_, e)| **e <= 1.0).map(|(d, _)| *d).last() {
        if let Some(i) = f.dates.iter().position(|d| *d > lu) {
            f = f.slice_from(i);
        }
    }
    if f.flow[0] == 0.0 {
        f.flow[0] = f.equity[0];
    }
    f.funded = f.equity.iter().any(|e| *e > 1.0);
    Ok(f)
}

/// Modified-Dietz daily returns (flows at start of day) -> time-weighted index.
pub fn add_returns(f: &mut EqFrame) {
    let n = f.len();
    f.pnl = vec![0.0; n];
    f.ret = vec![0.0; n];
    for i in 0..n {
        let prev = if i == 0 { 0.0 } else { f.equity[i - 1] };
        let base = prev + f.flow[i];
        let pnl = f.equity[i] - prev - f.flow[i];
        f.pnl[i] = pnl;
        f.ret[i] = if base > 1e-6 { pnl / base } else { 0.0 };
    }
    if n > 0 && f.pnl[0].abs() < 1e-9 {
        f.ret[0] = 0.0;
    }
    let mut acc = 1.0;
    f.twr = f
        .ret
        .iter()
        .map(|r| {
            acc *= 1.0 + r;
            acc
        })
        .collect();
    f.net_deposits = cumsum(&f.flow);
    f.cum_pnl = f.equity.iter().zip(&f.net_deposits).map(|(e, d)| e - d).collect();
    let pk = cummax(&f.twr);
    f.dd = f.twr.iter().zip(&pk).map(|(t, p)| t / p - 1.0).collect();
    let pk = cummax(&f.cum_pnl);
    f.dd_dollar = f.cum_pnl.iter().zip(&pk).map(|(c, p)| c - p).collect();
}

#[derive(Clone, Debug, Default)]
pub struct BenchFrame {
    pub price: Vec<f64>,
    pub value: Vec<f64>,
    pub cum_pnl: Vec<f64>,
    pub ret: Vec<f64>,
    pub twr: Vec<f64>,
    pub dd: Vec<f64>,
}

/// Put every deposit into the benchmark at that day's close (same cash flows).
pub fn benchmark_frame(df: &EqFrame, closes: &Closes) -> BenchFrame {
    let mut px: Vec<f64> = df.dates.iter().map(|d| last_le(closes, *d).unwrap_or(NAN)).collect();
    // bfill
    let mut next = NAN;
    for p in px.iter_mut().rev() {
        if p.is_nan() {
            *p = next;
        } else {
            next = *p;
        }
    }
    let mut sh = 0.0;
    let mut value = Vec::new();
    for (i, p) in px.iter().enumerate() {
        sh += df.flow[i] / p;
        value.push(sh * p);
    }
    let dep = cumsum(&df.flow);
    let cum_pnl = value.iter().zip(&dep).map(|(v, d)| v - d).collect();
    let mut ret = vec![0.0; px.len()];
    for i in 1..px.len() {
        let r = px[i] / px[i - 1] - 1.0;
        ret[i] = if r.is_nan() { 0.0 } else { r };
    }
    let twr: Vec<f64> = px.iter().map(|p| p / px[0]).collect();
    let pk = cummax(&twr);
    let dd = twr.iter().zip(&pk).map(|(t, p)| t / p - 1.0).collect();
    BenchFrame { price: px, value, cum_pnl, ret, twr, dd }
}

#[derive(Clone, Debug)]
pub struct PerfStats {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub days: usize,
    pub total_return: f64,
    pub cagr: f64,
    pub vol: f64,
    pub sharpe: f64,
    pub sortino: f64,
    pub max_dd: f64,
    pub max_dd_dollar: f64,
    pub calmar: f64,
    pub best_day: f64,
    pub worst_day: f64,
    pub pct_up_days: f64,
    pub net_deposits: f64,
    pub pnl: f64,
    pub ending_equity: f64,
    pub beta: f64,
    pub alpha: f64,
    pub correlation: f64,
    pub up_capture: f64,
    pub down_capture: f64,
    pub tracking_error: f64,
    pub info_ratio: f64,
}

/// Inputs perf_stats reads, so benchmark frames can be scored the same way.
pub struct StatInput<'a> {
    pub dates: &'a [NaiveDate],
    pub ret: &'a [f64],
    pub twr: &'a [f64],
    pub dd: &'a [f64],
    pub dd_dollar: &'a [f64],
    pub flow: &'a [f64],
    pub cum_pnl: &'a [f64],
    pub equity: &'a [f64],
}

impl<'a> StatInput<'a> {
    pub fn of(f: &'a EqFrame) -> StatInput<'a> {
        StatInput {
            dates: &f.dates,
            ret: &f.ret,
            twr: &f.twr,
            dd: &f.dd,
            dd_dollar: &f.dd_dollar,
            flow: &f.flow,
            cum_pnl: &f.cum_pnl,
            equity: &f.equity,
        }
    }
}

pub fn perf_stats(df: &StatInput, bench: Option<&BenchFrame>, rf: f64) -> PerfStats {
    let r = &df.ret[1..];
    let n = r.len();
    let span = (*df.dates.last().unwrap() - df.dates[0]).num_days() as f64;
    let years = (span / 365.25).max(1.0 / 365.25);
    let total = df.twr.last().unwrap() - 1.0;
    let cagr = if total > -1.0 { (1.0 + total).powf(1.0 / years) - 1.0 } else { -1.0 };
    let sd = std1(r);
    let vol = if n > 1 { sd * TRADING_DAYS.sqrt() } else { NAN };
    let ex: Vec<f64> = r.iter().map(|x| x - rf / TRADING_DAYS).collect();
    let sharpe = if n > 1 && sd > 0.0 { mean(&ex) / sd * TRADING_DAYS.sqrt() } else { NAN };
    let downside = if n > 0 {
        let d: Vec<f64> = ex.iter().map(|x| x.min(0.0).powi(2)).collect();
        mean(&d).sqrt() * TRADING_DAYS.sqrt()
    } else {
        NAN
    };
    let sortino = if downside != 0.0 && downside > 0.0 { mean(&ex) * TRADING_DAYS / downside } else { NAN };
    let mdd = util::vmin(df.dd);
    let mut st = PerfStats {
        start: df.dates[0],
        end: *df.dates.last().unwrap(),
        days: n,
        total_return: total,
        cagr,
        vol,
        sharpe,
        sortino,
        max_dd: mdd,
        max_dd_dollar: util::vmin(df.dd_dollar),
        calmar: if mdd < 0.0 { cagr / mdd.abs() } else { NAN },
        best_day: if n > 0 { util::vmax(r) } else { NAN },
        worst_day: if n > 0 { util::vmin(r) } else { NAN },
        pct_up_days: if n > 0 { r.iter().filter(|x| **x > 0.0).count() as f64 / n as f64 } else { NAN },
        net_deposits: df.flow.iter().sum(),
        pnl: *df.cum_pnl.last().unwrap(),
        ending_equity: *df.equity.last().unwrap(),
        beta: NAN,
        alpha: NAN,
        correlation: NAN,
        up_capture: NAN,
        down_capture: NAN,
        tracking_error: NAN,
        info_ratio: NAN,
    };
    if let Some(b) = bench {
        if n > 5 {
            let b: Vec<f64> = b.ret[1..].iter().map(|x| if x.is_nan() { 0.0 } else { *x }).collect();
            if util::var1(&b) > 0.0 {
                let c01 = util::cov1(r, &b);
                let c11 = util::var1(&b);
                st.beta = c01 / c11;
                st.alpha = (mean(r) - st.beta * mean(&b)) * TRADING_DAYS;
                st.correlation = c01 / (std1(r) * std1(&b));
                let up: Vec<usize> = (0..n).filter(|&i| b[i] > 0.0).collect();
                let dn: Vec<usize> = (0..n).filter(|&i| b[i] < 0.0).collect();
                let m = |ix: &[usize], v: &[f64]| mean(&ix.iter().map(|&i| v[i]).collect::<Vec<_>>());
                if !up.is_empty() && m(&up, &b) != 0.0 {
                    st.up_capture = m(&up, r) / m(&up, &b);
                }
                if !dn.is_empty() && m(&dn, &b) != 0.0 {
                    st.down_capture = m(&dn, r) / m(&dn, &b);
                }
                let active: Vec<f64> = (0..n).map(|i| r[i] - b[i]).collect();
                st.tracking_error = std1(&active) * TRADING_DAYS.sqrt();
                st.info_ratio = if st.tracking_error != 0.0 && !st.tracking_error.is_nan() {
                    mean(&active) * TRADING_DAYS / st.tracking_error
                } else if st.tracking_error.is_nan() {
                    mean(&active) * TRADING_DAYS / st.tracking_error
                } else {
                    NAN
                };
            }
        }
    }
    st
}

#[derive(Clone, Debug)]
pub struct Drawdown {
    pub peak: NaiveDate,
    pub trough: NaiveDate,
    pub recovered: Option<NaiveDate>,
    pub depth: f64,
    pub length_days: i64,
}

/// Largest peak-to-trough episodes on the time-weighted index.
pub fn drawdown_periods(df: &EqFrame, top: usize) -> Vec<Drawdown> {
    let twr = &df.twr;
    let peak = cummax(twr);
    let n = twr.len();
    let mut rows = Vec::new();
    let mut start: Option<usize> = None;
    for i in 0..n {
        let u = twr[i] < peak[i] - 1e-12;
        if u && start.is_none() {
            start = Some(i);
        }
        if (!u || i == n - 1) && start.is_some() {
            let s = start.unwrap();
            let mut tr = s;
            for j in s..=i {
                if twr[j] < twr[tr] {
                    tr = j;
                }
            }
            let mut pk = 0;
            for j in 0..=s {
                if twr[j] > twr[pk] {
                    pk = j;
                }
            }
            rows.push(Drawdown {
                peak: df.dates[pk],
                trough: df.dates[tr],
                recovered: if !u { Some(df.dates[i]) } else { None },
                depth: twr[tr] / peak[tr] - 1.0,
                length_days: (df.dates[i] - df.dates[pk]).num_days(),
            });
            start = None;
        }
    }
    rows.sort_by(|a, b| a.depth.partial_cmp(&b.depth).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(top);
    rows
}

#[derive(Clone, Debug, Default)]
pub struct Rolling {
    pub vol: Vec<f64>,
    pub sharpe: Vec<f64>,
    pub beta: Option<Vec<f64>>,
}

pub fn rolling_stats(df: &EqFrame, bench: Option<&BenchFrame>, w: usize) -> Rolling {
    let r = &df.ret;
    let n = r.len();
    let mut out = Rolling { vol: vec![NAN; n], sharpe: vec![NAN; n], beta: None };
    let b: Option<Vec<f64>> = bench.map(|b| b.ret.iter().map(|x| if x.is_nan() { 0.0 } else { *x }).collect());
    let mut beta = vec![NAN; n];
    for i in 0..n {
        if i + 1 < w {
            continue;
        }
        let win = &r[i + 1 - w..=i];
        let sd = std1(win);
        out.vol[i] = sd * TRADING_DAYS.sqrt();
        out.sharpe[i] = mean(win) / sd * TRADING_DAYS.sqrt();
        if let Some(b) = &b {
            let bw = &b[i + 1 - w..=i];
            beta[i] = util::cov1(win, bw) / util::var1(bw);
        }
    }
    if b.is_some() {
        out.beta = Some(beta);
    }
    out
}

#[derive(Clone, Debug)]
pub struct Month {
    pub year: i32,
    pub month: u32,
    pub ret: f64,
    pub pnl: f64,
}

/// Calendar-month compounding of a daily return series (every month in the span, empty ones = 0).
pub fn monthly(dates: &[NaiveDate], ret: &[f64], pnl: Option<&[f64]>) -> Vec<Month> {
    if dates.is_empty() {
        return vec![];
    }
    let (mut y, mut m) = (dates[0].year(), dates[0].month());
    let (ly, lm) = (dates.last().unwrap().year(), dates.last().unwrap().month());
    let mut out = Vec::new();
    loop {
        let mut prod = 1.0;
        let mut s = 0.0;
        for (i, d) in dates.iter().enumerate() {
            if d.year() == y && d.month() == m {
                if !ret[i].is_nan() {
                    prod *= 1.0 + ret[i];
                }
                if let Some(p) = pnl {
                    if !p[i].is_nan() {
                        s += p[i];
                    }
                }
            }
        }
        out.push(Month { year: y, month: m, ret: prod - 1.0, pnl: s });
        if y == ly && m == lm {
            break;
        }
        m += 1;
        if m == 13 {
            m = 1;
            y += 1;
        }
    }
    out
}
