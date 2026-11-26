//! Alternative readings of an open multi-leg position: every partition of the legs (and 100-share
//! stock lots) into groups that are each a recognised strategy.

use std::collections::HashSet;

use chrono::NaiveDate;
use indexmap::IndexMap;
use serde_json::Value;

use crate::jv;
use crate::ledger::{classify, Combo};
use crate::occ::parse_occ;
use crate::optmath::{payoff_curves, LegIn};
use crate::util::{g, linspace};

pub const STOCK: &str = "STOCK";
pub const MAX_UNITS: usize = 8;
pub const MAX_READINGS: usize = 40;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Greeks {
    pub delta: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
}

impl Greeks {
    pub fn is_empty(&self) -> bool {
        self.delta.is_none() && self.theta.is_none() && self.vega.is_none()
    }
    fn add(&mut self, o: &Greeks) {
        let a = |x: &mut Option<f64>, y: Option<f64>| {
            if let Some(y) = y {
                *x = Some(x.unwrap_or(0.0) + y);
            }
        };
        a(&mut self.delta, o.delta);
        a(&mut self.theta, o.theta);
        a(&mut self.vega, o.vega);
    }
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub symbol: String,
    pub qty: f64,
    pub cost: f64,
    pub value: f64,
    pub greeks: Greeks,
    pub iv: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub strategy: String,
    pub units: Vec<Unit>,
    pub defined_risk: bool,
    pub max_gain: f64,
    pub max_loss: f64,
    pub breakevens: Vec<f64>,
    pub zones: Vec<(f64, f64)>,
    pub cost: f64,
    pub value: f64,
    pub greeks: Greeks,
    pub curve_x: Vec<f64>,
    pub curve_expiry: Vec<f64>,
    pub curve_today: Vec<f64>,
}

#[derive(PartialEq, PartialOrd, Clone, Debug)]
pub struct LegSort(NaiveDate, String, f64);

pub fn leg_sort(sym: &str) -> LegSort {
    if sym == STOCK {
        return LegSort(NaiveDate::MIN, String::new(), 0.0);
    }
    let c = parse_occ(sym).unwrap();
    LegSort(c.expiry, c.right.to_string(), c.strike)
}

fn cmp_ls(a: &LegSort, b: &LegSort) -> std::cmp::Ordering {
    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
}

impl Group {
    pub fn unrealized(&self) -> f64 {
        self.value - self.cost
    }
    pub fn legs_label(&self) -> String {
        legs_label(&self.units)
    }
}

pub fn legs_label(units: &[Unit]) -> String {
    let mut merged: IndexMap<String, f64> = IndexMap::new();
    for u in units {
        *merged.entry(u.symbol.clone()).or_insert(0.0) += u.qty;
    }
    let mut items: Vec<(String, f64)> = merged.into_iter().collect();
    items.sort_by(|a, b| cmp_ls(&leg_sort(&a.0), &leg_sort(&b.0)));
    items
        .iter()
        .map(|(sym, q)| {
            let sign = if *q > 0.0 { "+" } else { "−" };
            if sym == STOCK {
                format!("{}{} sh", sign, g(q.abs()))
            } else {
                let c = parse_occ(sym).unwrap();
                format!("{}{} {} {}{}", sign, g(q.abs()), c.expiry.format("%m/%d"), g(c.strike), c.right)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Debug)]
pub struct Reading {
    pub groups: Vec<Group>,
    pub as_opened: bool,
}

impl Reading {
    pub fn label(&self) -> String {
        self.groups.iter().map(|g| g.strategy.as_str()).collect::<Vec<_>>().join(" + ")
    }
    pub fn undefined(&self) -> usize {
        self.groups.iter().filter(|g| !g.defined_risk).count()
    }
    pub fn sum_max_loss(&self) -> f64 {
        self.groups.iter().map(|g| g.max_loss).sum()
    }
}

fn set_partitions(n: usize, start: usize) -> Vec<Vec<Vec<usize>>> {
    if start >= n {
        return vec![vec![]];
    }
    let mut out = Vec::new();
    for part in set_partitions(n, start + 1) {
        for i in 0..part.len() {
            let mut p = part.clone();
            let mut blk = vec![start];
            blk.extend_from_slice(&part[i]);
            p[i] = blk;
            out.push(p);
        }
        let mut p = vec![vec![start]];
        p.extend(part.into_iter());
        out.push(p);
    }
    out
}

pub fn name_group(units: &[&Unit]) -> String {
    let mut legs: IndexMap<String, f64> = IndexMap::new();
    let mut shares = 0.0;
    for u in units {
        if u.symbol == STOCK {
            shares += u.qty;
        } else {
            *legs.entry(u.symbol.clone()).or_insert(0.0) += u.qty;
        }
    }
    legs.retain(|_, v| v.abs() > 1e-9);
    if shares != 0.0 {
        if shares < 0.0 || legs.is_empty() {
            return if legs.is_empty() { "Stock".into() } else { String::new() };
        }
        let calls: Vec<f64> = legs.iter().filter(|(s, _)| parse_occ(s).unwrap().is_call()).map(|(_, q)| *q).collect();
        let puts: Vec<f64> = legs.iter().filter(|(s, _)| !parse_occ(s).unwrap().is_call()).map(|(_, q)| *q).collect();
        let lots = shares / 100.0;
        if calls.len() == 1 && puts.is_empty() && calls[0] == -lots {
            return "Covered call".into();
        }
        if puts.len() == 1 && calls.is_empty() && puts[0] == lots {
            return "Protective put".into();
        }
        if calls.len() == 1 && puts.len() == 1 && calls[0] == -lots && puts[0] == lots {
            return "Collar".into();
        }
        return String::new();
    }
    let name = classify(&Combo::bare(legs));
    if name.starts_with("Custom") || name == "Unknown" || trivial(&name) {
        return String::new();
    }
    name
}

fn trivial(name: &str) -> bool {
    ["Long calls (", "Long puts (", "Short calls (", "Short puts ("].iter().any(|p| name.starts_with(p))
}

pub fn evaluate(units: &[&Unit], strategy: &str, spot: f64, today: NaiveDate, xs: &[f64], risk_xs: &[f64]) -> Group {
    let legs: Vec<LegIn> = units
        .iter()
        .filter(|u| u.symbol != STOCK)
        .map(|u| LegIn { symbol: u.symbol.clone(), qty: u.qty, cost: u.cost, iv: u.iv })
        .collect();
    let shares: f64 = units.iter().filter(|u| u.symbol == STOCK).map(|u| u.qty).sum();
    let share_cost: f64 = units.iter().filter(|u| u.symbol == STOCK).map(|u| u.cost).sum();
    let shown = payoff_curves(&legs, spot, today, shares, share_cost, xs);
    let risk = payoff_curves(&legs, spot, today, shares, share_cost, risk_xs);
    // exact, from the strikes (see optmath::exact_expiry)
    let (max_gain, max_loss) = (risk.max_profit, risk.max_loss);
    let mut greeks = Greeks::default();
    for u in units {
        greeks.add(&u.greeks);
    }
    Group {
        strategy: strategy.to_string(),
        units: units.iter().map(|u| (*u).clone()).collect(),
        defined_risk: !max_loss.is_infinite(),
        max_gain,
        max_loss,
        breakevens: risk.breakevens,
        zones: risk.zones,
        cost: units.iter().map(|u| u.cost).sum(),
        value: bounded_value(units, max_gain, max_loss),
        greeks,
        curve_x: shown.x,
        curve_expiry: shown.expiry,
        curve_today: shown.today,
    }
}

pub fn grid(units: &[&Unit], spot: f64) -> (Vec<f64>, Vec<f64>) {
    let mut strikes: Vec<f64> = units.iter().filter(|u| u.symbol != STOCK).map(|u| parse_occ(&u.symbol).unwrap().strike).collect();
    strikes.push(spot);
    let lo = strikes.iter().cloned().fold(f64::INFINITY, f64::min) * 0.85;
    let hi = strikes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    // the payoff bends only at strikes: put every strike on both grids so the corners are drawn exactly
    let with_strikes = |mut v: Vec<f64>| {
        let (a, b) = (v[0], v[v.len() - 1]);
        v.extend(strikes.iter().copied().filter(|k| *k > a && *k < b));
        v.sort_by(|x, y| x.partial_cmp(y).unwrap());
        v.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
        v
    };
    (with_strikes(linspace(lo, hi * 1.15, 241)), with_strikes(linspace(0.01, hi * 3.0, 1201)))
}

#[derive(Clone, Debug)]
pub struct Whole {
    pub x: Vec<f64>,
    pub expiry: Vec<f64>,
    pub today: Vec<f64>,
    pub max_gain: f64,
    pub max_loss: f64,
    pub breakevens: Vec<f64>,
    pub zones: Vec<(f64, f64)>,
    pub unrealized: f64,
    pub greeks: Greeks,
}

type Key = Vec<(String, i8, usize)>;

fn key(units: &[&Unit]) -> Key {
    let mut c: Vec<((String, i8), usize)> = Vec::new();
    for u in units {
        let k = (u.symbol.clone(), if u.qty > 0.0 { 1 } else { -1 });
        match c.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => e.1 += 1,
            None => c.push((k, 1)),
        }
    }
    let mut v: Vec<(String, i8, usize)> = c.into_iter().map(|((s, sg), n)| (s, sg, n)).collect();
    v.sort();
    v
}

pub fn readings(units: &[Unit], spot: f64, today: NaiveDate, opened: Option<&[Vec<String>]>) -> (Vec<Reading>, Whole) {
    let all: Vec<&Unit> = units.iter().collect();
    let (xs, risk_xs) = grid(&all, spot);
    let whole = evaluate(&all, "Whole position", spot, today, &xs, &risk_xs);
    let mut seen: HashSet<Vec<Key>> = HashSet::new();
    let mut out: Vec<(Vec<Key>, Vec<Vec<&Unit>>, Vec<String>)> = Vec::new();
    for part in set_partitions(units.len(), 0) {
        let groups: Vec<Vec<&Unit>> = part.iter().map(|g| g.iter().map(|&i| &units[i]).collect()).collect();
        let mut canon: Vec<Key> = groups.iter().map(|g| key(g)).collect();
        canon.sort();
        if seen.contains(&canon) {
            continue;
        }
        seen.insert(canon.clone());
        let pure: Vec<&str> = groups
            .iter()
            .filter(|g| g.iter().map(|u| u.symbol.as_str()).collect::<HashSet<_>>().len() == 1)
            .map(|g| g[0].symbol.as_str())
            .collect();
        if pure.iter().collect::<HashSet<_>>().len() != pure.len() {
            continue;
        }
        let names: Vec<String> = groups.iter().map(|g| name_group(g)).collect();
        if names.iter().any(|n| n.is_empty()) {
            continue;
        }
        out.push((canon, groups, names));
    }
    let mut opened_canon: Option<Vec<Key>> = None;
    if let Some(opened) = opened.filter(|o| !o.is_empty()) {
        let mut pool: Vec<&Unit> = units.iter().collect();
        let mut ogroups: Vec<Vec<&Unit>> = Vec::new();
        let mut ok = true;
        'outer: for syms in opened {
            let mut gq = Vec::new();
            for s in syms {
                match pool.iter().position(|u| &u.symbol == s) {
                    Some(i) => gq.push(pool.remove(i)),
                    None => {
                        ok = false;
                        break 'outer;
                    }
                }
            }
            ogroups.push(gq);
        }
        if ok {
            for u in pool {
                ogroups.push(vec![u]);
            }
            let mut c: Vec<Key> = ogroups.iter().map(|g| key(g)).collect();
            c.sort();
            opened_canon = Some(c);
        }
    }
    let mut rs: Vec<Reading> = out
        .into_iter()
        .map(|(canon, groups, names)| {
            let mut gs: Vec<Group> = groups.iter().zip(&names).map(|(g, n)| evaluate(g, n, spot, today, &xs, &risk_xs)).collect();
            gs.sort_by(|a, b| {
                let ma = a.units.iter().map(|u| leg_sort(&u.symbol)).min_by(cmp_ls).unwrap();
                let mb = b.units.iter().map(|u| leg_sort(&u.symbol)).min_by(cmp_ls).unwrap();
                cmp_ls(&ma, &mb)
            });
            Reading { groups: gs, as_opened: opened_canon.as_ref() == Some(&canon) }
        })
        .collect();
    rs.sort_by(|a, b| {
        (!a.as_opened, a.undefined(), a.groups.len())
            .cmp(&(!b.as_opened, b.undefined(), b.groups.len()))
            .then_with(|| a.label().cmp(&b.label()))
    });
    rs.truncate(MAX_READINGS);
    let w = Whole {
        x: whole.curve_x.clone(),
        expiry: whole.curve_expiry.clone(),
        today: whole.curve_today.clone(),
        max_gain: whole.max_gain,
        max_loss: whole.max_loss,
        breakevens: whole.breakevens.clone(),
        zones: whole.zones.clone(),
        unrealized: whole.unrealized(),
        greeks: whole.greeks.clone(),
    };
    (rs, w)
}

pub struct OptRow {
    pub symbol: String,
    pub qty: f64,
    pub cost_basis: f64,
    pub market_value: f64,
}

/// Units of one contract / 100 shares when that keeps the count small. Returns (units, odd lot, split).
pub fn build_units(rows: &[OptRow], shares: f64, share_cost: f64, share_value: f64, snapshots: &Value) -> (Vec<Unit>, Option<Unit>, bool) {
    let total: f64 = rows.iter().map(|r| r.qty.abs()).sum::<f64>() + (shares.abs() / 100.0).floor();
    let split = total <= MAX_UNITS as f64;
    let mut units = Vec::new();
    for r in rows {
        let snap = snapshots.get(&r.symbol).unwrap_or(jv::empty_obj());
        let gk = jv::obj(snap.get("greeks")).filter(|o| !o.is_empty());
        let n = if split { r.qty.abs().trunc() as usize } else { 1 };
        let n = n.max(if split { 0 } else { 1 });
        if n == 0 {
            continue;
        }
        let q = r.qty / n as f64;
        let mult = 100.0 * q;
        for _ in 0..n {
            let greeks = match gk {
                Some(o) => {
                    let gv = |k: &str| o.get(k).and_then(jv::float_of).unwrap_or(0.0) * mult;
                    Greeks { delta: Some(gv("delta")), theta: Some(gv("theta")), vega: Some(gv("vega")) }
                }
                None => Greeks::default(),
            };
            units.push(Unit {
                symbol: r.symbol.clone(),
                qty: q,
                cost: r.cost_basis / n as f64,
                value: r.market_value / n as f64,
                greeks,
                iv: jv::f(snap, "impliedVolatility"),
            });
        }
    }
    let sign = if shares >= 0.0 { 1.0 } else { -1.0 };
    let lots = (shares.abs() / 100.0).floor() as usize;
    let psc = if shares != 0.0 { share_cost / shares } else { 0.0 };
    let psv = if shares != 0.0 { share_value / shares } else { 0.0 };
    let stock = |q: f64| Unit { symbol: STOCK.into(), qty: q, cost: q * psc, value: q * psv, greeks: Greeks { delta: Some(q), ..Default::default() }, iv: None };
    if split {
        for _ in 0..lots {
            units.push(stock(100.0 * sign));
        }
    } else if lots > 0 {
        units.push(stock(100.0 * lots as f64 * sign));
    }
    let rest = shares - 100.0 * lots as f64 * sign;
    let odd = if rest.abs() > 1e-9 { Some(stock(rest)) } else { None };
    (units, odd, split)
}

/// A group's market value, kept within what its legs can be worth: between its worst and best case at
/// expiration (for legs that all expire together; calendars and stock-only groups aren't bounded).
/// Marks on wide quotes can otherwise price e.g. a long put spread below zero.
fn bounded_value(units: &[&Unit], max_gain: f64, max_loss: f64) -> f64 {
    let v: f64 = units.iter().map(|u| u.value).sum();
    let cost: f64 = units.iter().map(|u| u.cost).sum();
    let exps: std::collections::BTreeSet<_> = units.iter().filter_map(|u| parse_occ(&u.symbol).map(|c| c.expiry)).collect();
    if exps.len() != 1 || v.is_nan() {
        return v;
    }
    let lo = if max_loss.is_finite() { max_loss + cost } else { f64::NEG_INFINITY };
    let hi = if max_gain.is_finite() { max_gain + cost } else { f64::INFINITY };
    if lo <= hi { v.clamp(lo, hi) } else { v }
}
