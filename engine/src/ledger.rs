//! Raw Alpaca activities -> normalized events -> FIFO lots, closed trades and multi-leg option
//! strategies ("combos").
//!
//! * ``qty`` on events is the signed position delta (+ buys / - sells).
//! * Expirations, assignments and exercises close the option at price 0; the stock leg of an
//!   assignment arrives separately (``OPTRD``) at the strike.
//! * Cash is signed from the account's point of view (+ = cash in).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::NaiveDate;
use indexmap::IndexMap;
use serde_json::Value;

use crate::jv;
use crate::occ::{parse_occ, Contract};
use crate::util::{self, from_ny, g, ny_date, parse_ts, Ts};

pub const EPS: f64 = 1e-9;

const FLOW_TYPES: [&str; 5] = ["CSD", "CSW", "JNLC", "ACATC", "JNL"];
const FEE_TYPES: [&str; 4] = ["FEE", "CFEE", "PTC", "DIVFEE"];
const INCOME_TYPES: [&str; 10] = ["DIV", "DIVCGL", "DIVCGS", "DIVNRA", "DIVROC", "DIVTW", "DIVTXEX", "DIVFT", "CGD", "PTR"];
const INTEREST_TYPES: [&str; 3] = ["INT", "INTNRA", "INTTW"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Trade,
    Expired,
    Assigned,
    Exercised,
    Delivery,
    Split,
    Spin,
    TransferIn,
    Flow,
    Fee,
    Income,
    Interest,
    Other,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Trade => "trade",
            Kind::Expired => "expired",
            Kind::Assigned => "assigned",
            Kind::Exercised => "exercised",
            Kind::Delivery => "delivery",
            Kind::Split => "split",
            Kind::Spin => "spin",
            Kind::TransferIn => "transfer_in",
            Kind::Flow => "flow",
            Kind::Fee => "fee",
            Kind::Income => "income",
            Kind::Interest => "interest",
            Kind::Other => "other",
        }
    }
    pub fn is_position(&self) -> bool {
        matches!(self, Kind::Trade | Kind::Delivery | Kind::Expired | Kind::Assigned | Kind::Exercised)
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub seq: usize,
    pub time: Ts,
    pub kind: Kind,
    pub symbol: String,
    pub underlying: String,
    pub is_option: bool,
    pub qty: f64,
    pub price: f64,
    pub cash: f64,
    pub multiplier: f64,
    pub order_id: Option<String>,
    pub raw_type: String,
    pub sub_type: String,
    pub fee_date: Option<NaiveDate>,
    pub gross_price: f64,
    pub fee: f64,
    pub allocated: bool,
}

impl Event {
    #[allow(clippy::too_many_arguments)]
    fn new(seq: usize, time: Ts, kind: Kind, symbol: &str, qty: f64, price: f64, cash: f64, mult: f64) -> Event {
        let c = parse_occ(symbol);
        Event {
            seq,
            time,
            kind,
            symbol: symbol.to_string(),
            underlying: c.as_ref().map(|c| c.underlying.clone()).unwrap_or_else(|| symbol.to_string()),
            is_option: c.is_some(),
            qty,
            price,
            cash,
            multiplier: mult,
            order_id: None,
            raw_type: String::new(),
            sub_type: String::new(),
            fee_date: None,
            gross_price: f64::NAN,
            fee: 0.0,
            allocated: false,
        }
    }
    pub fn ny_date(&self) -> NaiveDate {
        ny_date(&self.time)
    }
}

fn close_of(d: NaiveDate, h: u32, m: u32, s: u32) -> Ts {
    from_ny(d, h, m, s)
}

pub fn normalize(activities: &[Value]) -> Vec<Event> {
    let mut evs = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (i, a) in activities.iter().enumerate() {
        if let Some(id) = jv::opt_s(a, "id") {
            if !seen.insert(id) {
                continue;
            }
        }
        let t = jv::s(a, "activity_type").to_uppercase();
        let sym = jv::s(a, "symbol");
        if t == "FILL" {
            let side = jv::s(a, "side").to_lowercase();
            let mut q = jv::f0(a, "qty");
            if side != "buy" {
                q = -q;
            }
            let px = jv::f0(a, "price");
            let mult = if parse_occ(&sym).is_some() { 100.0 } else { 1.0 };
            let Some(time) = parse_ts(&jv::s(a, "transaction_time")) else { continue };
            let mut e = Event::new(i, time, Kind::Trade, &sym, q, px, -q * px * mult, mult);
            e.order_id = jv::opt_s(a, "order_id");
            e.raw_type = t;
            e.gross_price = px;
            evs.push(e);
            continue;
        }
        let ds = jv::opt_s(a, "date").or_else(|| jv::opt_s(a, "created_at")).unwrap_or_default();
        let Some(d) = ds.get(..10).and_then(|x| NaiveDate::parse_from_str(x, "%Y-%m-%d").ok()) else { continue };
        let created = jv::opt_s(a, "created_at").and_then(|x| parse_ts(&x));
        let net = jv::f0(a, "net_amount");
        let qty = jv::f0(a, "qty");
        let price = jv::f0(a, "price");
        let mut e = match t.as_str() {
            "OPEXP" | "OPASN" | "OPEXC" => {
                let kind = match t.as_str() {
                    "OPEXP" => Kind::Expired,
                    "OPASN" => Kind::Assigned,
                    _ => Kind::Exercised,
                };
                let when = parse_occ(&sym).map(|c| c.expiry.min(d)).unwrap_or(d);
                Event::new(i, close_of(when, 16, 0, 1), kind, &sym, qty, 0.0, net, 100.0)
            }
            "OPTRD" => {
                let px = if price != 0.0 { price } else if qty != 0.0 { (net / qty).abs() } else { 0.0 };
                let cash = if net != 0.0 { net } else { -qty * px };
                Event::new(i, close_of(d, 16, 0, 2), Kind::Delivery, &sym, qty, px, cash, 1.0)
            }
            "SPLIT" | "MA" | "NC" | "REORG" => {
                let mut e = Event::new(i, created.unwrap_or_else(|| close_of(d, 9, 0, 0)), Kind::Split, &sym, qty, price, net, 1.0);
                e.sub_type = jv::s(a, "activity_sub_type");
                e
            }
            "SPIN" => Event::new(i, created.unwrap_or_else(|| close_of(d, 9, 0, 0)), Kind::Spin, &sym, qty, price, net, 1.0),
            "JNLS" | "ACATS" => Event::new(i, created.unwrap_or_else(|| close_of(d, 16, 0, 0)), Kind::TransferIn, &sym, qty, price, 0.0, 1.0),
            x if FLOW_TYPES.contains(&x) => Event::new(i, close_of(d, 9, 0, 0), Kind::Flow, "", 0.0, 0.0, net, 1.0),
            x if FEE_TYPES.contains(&x) => {
                let mut e = Event::new(i, close_of(d, 17, 0, 0), Kind::Fee, &sym, 0.0, 0.0, net, 1.0);
                e.order_id = jv::opt_s(a, "order_id");
                e.sub_type = jv::s(a, "activity_sub_type").to_uppercase();
                e.fee_date = Some(d);
                e
            }
            x if INCOME_TYPES.contains(&x) => Event::new(i, close_of(d, 12, 0, 0), Kind::Income, &sym, 0.0, 0.0, net, 1.0),
            x if INTEREST_TYPES.contains(&x) => Event::new(i, close_of(d, 17, 0, 0), Kind::Interest, &sym, 0.0, 0.0, net, 1.0),
            _ => Event::new(i, close_of(d, 17, 0, 0), Kind::Other, &sym, 0.0, 0.0, net, 1.0),
        };
        e.raw_type = t;
        evs.push(e);
    }
    evs.sort_by(|a, b| a.time.cmp(&b.time).then(a.seq.cmp(&b.seq)));
    evs
}

// ------------------------------------------------------------------------------------ fees
fn fee_weights(sub: &str, evs: &[Event], fills: &[usize]) -> Vec<f64> {
    let w: Vec<f64> = fills
        .iter()
        .map(|&i| {
            let f = &evs[i];
            let notional = (f.qty * f.gross_price * f.multiplier).abs();
            let contracts = if f.is_option { f.qty.abs() } else { 0.0 };
            match sub {
                "TAF" | "REG" => {
                    if f.qty < 0.0 {
                        notional
                    } else {
                        0.0
                    }
                }
                "ORF" | "OCC" => contracts,
                "CAT" => f.qty.abs(),
                _ => notional,
            }
        })
        .collect();
    if w.iter().sum::<f64>() > 0.0 {
        w
    } else {
        vec![1.0; fills.len()]
    }
}

fn apply_fee(evs: &mut [Event], fills: &[usize], weights: &[f64], amount: f64) {
    let tot: f64 = weights.iter().sum();
    for (&i, &w) in fills.iter().zip(weights) {
        if w > 0.0 {
            evs[i].fee += amount * w / tot;
        }
    }
}

fn prev_day_with_fills(by_date: &BTreeMap<NaiveDate, Vec<usize>>, d: NaiveDate) -> Option<NaiveDate> {
    by_date.keys().filter(|x| **x <= d && (d - **x).num_days() <= 4).max().copied()
}

/// Fold fees into the fills they belong to, so lot prices are effective (cost / qty).
pub fn allocate_fees(evs: &mut [Event], daily_fallback: &BTreeMap<NaiveDate, f64>) {
    let trades: Vec<usize> = (0..evs.len()).filter(|&i| evs[i].kind == Kind::Trade).collect();
    let mut by_order: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_date: BTreeMap<NaiveDate, Vec<usize>> = BTreeMap::new();
    for &i in &trades {
        if let Some(o) = &evs[i].order_id {
            by_order.entry(o.clone()).or_default().push(i);
        }
        by_date.entry(evs[i].ny_date()).or_default().push(i);
    }
    let fees: Vec<usize> = (0..evs.len()).filter(|&i| evs[i].kind == Kind::Fee).collect();
    for &fi in &fees {
        let (oid, cash, sym, sub, fd) = {
            let fe = &evs[fi];
            (fe.order_id.clone(), fe.cash, fe.symbol.clone(), fe.sub_type.clone(), fe.fee_date.unwrap_or_else(|| fe.ny_date()))
        };
        if let Some(fills) = oid.as_ref().and_then(|o| by_order.get(o)) {
            let fills = fills.clone();
            let w: Vec<f64> = fills.iter().map(|&i| evs[i].qty.abs()).collect();
            apply_fee(evs, &fills, &w, cash);
            evs[fi].allocated = true;
            continue;
        }
        let mut fills: Vec<usize> = match by_date.get(&fd) {
            Some(v) => v.clone(),
            None => prev_day_with_fills(&by_date, fd).and_then(|p| by_date.get(&p)).cloned().unwrap_or_default(),
        };
        if !sym.is_empty() {
            let m: Vec<usize> = fills.iter().copied().filter(|&i| evs[i].underlying == sym).collect();
            if !m.is_empty() {
                fills = m;
            }
        }
        if !fills.is_empty() {
            let w = fee_weights(&sub, evs, &fills);
            apply_fee(evs, &fills, &w, cash);
            evs[fi].allocated = true;
        }
    }
    if fees.is_empty() {
        for (d, amt) in daily_fallback {
            let fills: Vec<usize> = match by_date.get(d) {
                Some(v) => v.clone(),
                None => prev_day_with_fills(&by_date, *d).and_then(|p| by_date.get(&p)).cloned().unwrap_or_default(),
            };
            if *amt != 0.0 && !fills.is_empty() {
                let w: Vec<f64> = fills.iter().map(|&i| if evs[i].is_option { evs[i].qty.abs() } else { 1.0 }).collect();
                apply_fee(evs, &fills, &w, *amt);
            }
        }
    }
    for &i in &trades {
        let e = &mut evs[i];
        if e.fee != 0.0 {
            e.cash += e.fee;
            e.price = -e.cash / (e.qty * e.multiplier);
        }
    }
}

// ------------------------------------------------------------------------------------ lots
#[derive(Clone, Debug)]
pub struct Lot {
    pub qty: f64,
    pub price: f64,
    pub time: Ts,
    pub multiplier: f64,
    pub combo_id: Option<usize>,
    pub origin: &'static str,
    pub fee_per_unit: f64,
    pub order_key: String,
}

#[derive(Clone, Debug)]
pub struct ClosedTrade {
    pub symbol: String,
    pub underlying: String,
    pub is_option: bool,
    pub long: bool,
    pub qty: f64,
    pub open_time: Ts,
    pub close_time: Ts,
    pub open_price: f64,
    pub close_price: f64,
    pub multiplier: f64,
    pub close_kind: &'static str,
    pub combo_id: Option<usize>,
    pub origin: &'static str,
    pub fees: f64,
    pub open_key: String,
    pub close_key: String,
}

impl ClosedTrade {
    pub fn pnl(&self) -> f64 {
        let sgn = if self.long { 1.0 } else { -1.0 };
        (self.close_price - self.open_price) * self.qty * self.multiplier * sgn
    }
    pub fn cost(&self) -> f64 {
        (self.open_price * self.qty * self.multiplier).abs()
    }
}

#[derive(Clone, Debug)]
pub struct Combo {
    pub id: usize,
    pub underlying: String,
    pub open_time: Ts,
    pub legs: IndexMap<String, f64>,
    pub open_cash: f64,
    pub last_open_time: Option<Ts>,
    pub rolled_from: Option<usize>,
    pub rolled_to: Option<usize>,
    pub stock_held_at_open: f64,
    pub strategy: String,
}

impl Combo {
    pub fn bare(legs: IndexMap<String, f64>) -> Combo {
        Combo {
            id: 0,
            underlying: String::new(),
            open_time: util::parse_ts("2000-01-01").unwrap(),
            legs,
            open_cash: 0.0,
            last_open_time: None,
            rolled_from: None,
            rolled_to: None,
            stock_held_at_open: 0.0,
            strategy: String::new(),
        }
    }
}

/// A closed trade after merging partial fills (one row of ``closed_df``).
#[derive(Clone, Debug)]
pub struct Trade {
    pub symbol: String,
    pub underlying: String,
    pub is_option: bool,
    pub long: bool,
    pub qty: f64, // NaN in the shared view
    pub open_time: Ts,
    pub close_time: Ts,
    pub open_price: f64,
    pub close_price: f64,
    pub multiplier: f64,
    pub close_kind: &'static str,
    pub combo_id: Option<usize>,
    pub pnl: f64,
    pub cost: f64,
    pub fees: f64,
    pub hold_days: f64,
    pub ret: f64,
}

impl Trade {
    pub fn asset(&self) -> &'static str {
        if self.is_option {
            "option"
        } else {
            "stock"
        }
    }
    pub fn direction(&self) -> &'static str {
        if self.long {
            "long"
        } else {
            "short"
        }
    }
}

#[derive(Clone, Debug)]
pub struct CashRow {
    pub date: NaiveDate,
    pub kind: Kind,
    pub symbol: String,
    pub cash: f64,
}

#[derive(Clone, Debug)]
pub struct Ledger {
    pub events: Vec<Event>,
    pub closed: Vec<ClosedTrade>,
    pub open_lots: IndexMap<String, Vec<Lot>>,
    pub combos: Vec<Combo>, // id = index + 1
    pub warnings: Vec<String>,
}

impl Ledger {
    pub fn combo(&self, id: usize) -> &Combo {
        &self.combos[id - 1]
    }

    /// Closed trades with partial fills merged (same opening order + same closing order = one trade).
    pub fn closed_df(&self) -> Vec<Trade> {
        type Key = (String, bool, String, String, &'static str, Option<usize>);
        let mut groups: IndexMap<Key, Vec<usize>> = IndexMap::new();
        for (i, c) in self.closed.iter().enumerate() {
            groups
                .entry((c.symbol.clone(), c.long, c.open_key.clone(), c.close_key.clone(), c.close_kind, c.combo_id))
                .or_default()
                .push(i);
        }
        let mut out: Vec<Trade> = groups
            .values()
            .map(|ix| {
                let f = &self.closed[ix[0]];
                let mut qty = 0.0;
                let mut oa = 0.0;
                let mut ca = 0.0;
                let mut pnl = 0.0;
                let mut cost = 0.0;
                let mut fees = 0.0;
                let mut ot = f.open_time;
                let mut ct = f.close_time;
                for &i in ix {
                    let c = &self.closed[i];
                    qty += c.qty;
                    oa += c.open_price * c.qty;
                    ca += c.close_price * c.qty;
                    pnl += c.pnl();
                    cost += c.cost();
                    fees += c.fees;
                    ot = ot.min(c.open_time);
                    ct = ct.max(c.close_time);
                }
                let hold = util::secs(&ot, &ct) / 86400.0;
                Trade {
                    symbol: f.symbol.clone(),
                    underlying: f.underlying.clone(),
                    is_option: f.is_option,
                    long: f.long,
                    qty,
                    open_time: ot,
                    close_time: ct,
                    open_price: oa / qty,
                    close_price: ca / qty,
                    multiplier: f.multiplier,
                    close_kind: f.close_kind,
                    combo_id: f.combo_id,
                    pnl,
                    cost,
                    fees,
                    hold_days: hold,
                    ret: if cost > 0.0 { pnl / cost } else { f64::NAN },
                }
            })
            .collect();
        out.sort_by(|a, b| a.close_time.cmp(&b.close_time));
        out
    }

    pub fn cash_rows(&self) -> Vec<CashRow> {
        self.events
            .iter()
            .filter(|e| e.cash != 0.0 || matches!(e.kind, Kind::Flow | Kind::Fee | Kind::Income | Kind::Interest))
            .map(|e| CashRow { date: e.ny_date(), kind: e.kind, symbol: e.symbol.clone(), cash: e.cash })
            .collect()
    }
}

struct Book {
    lots: IndexMap<String, VecDeque<Lot>>,
}

impl Book {
    fn get(&mut self, sym: &str) -> &mut VecDeque<Lot> {
        if !self.lots.contains_key(sym) {
            self.lots.insert(sym.to_string(), VecDeque::new());
        }
        self.lots.get_mut(sym).unwrap()
    }
    fn position(&mut self, sym: &str) -> f64 {
        self.get(sym).iter().map(|l| l.qty).sum()
    }
}

pub fn build_ledger(events: Vec<Event>, combo_window_s: f64) -> Ledger {
    let mut book = Book { lots: IndexMap::new() };
    let mut closed: Vec<ClosedTrade> = Vec::new();
    let mut combos: Vec<Combo> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut forming: HashMap<String, usize> = HashMap::new();
    let mut pending: IndexMap<String, Vec<usize>> = IndexMap::new();

    fn flush_split(sym: &str, pending: &mut IndexMap<String, Vec<usize>>, book: &mut Book, events: &[Event], warnings: &mut Vec<String>) {
        let Some(evs) = pending.shift_remove(sym) else { return };
        if evs.is_empty() {
            return;
        }
        let removed: f64 = -evs.iter().map(|&i| events[i].qty).filter(|q| *q < 0.0).sum::<f64>();
        let added: f64 = evs.iter().map(|&i| events[i].qty).filter(|q| *q > 0.0).sum();
        let held = book.position(sym);
        let ratio = if removed > EPS && added > EPS {
            added / removed
        } else if added > EPS && held.abs() > EPS {
            (held + added) / held
        } else if removed > EPS && held.abs() > EPS && removed < held {
            (held - removed) / held
        } else {
            let desc: Vec<String> = evs.iter().map(|&i| format!("({}, '{}')", util::py_float(events[i].qty), events[i].raw_type)).collect();
            warnings.push(format!("{}: could not infer split ratio from [{}]", sym, desc.join(", ")));
            return;
        };
        for l in book.get(sym).iter_mut() {
            l.qty *= ratio;
            l.price /= ratio;
        }
    }

    for idx in 0..events.len() {
        let ev = &events[idx];
        let keys: Vec<String> = pending.keys().cloned().collect();
        for s in keys {
            if s != ev.symbol || ev.kind != Kind::Split {
                flush_split(&s, &mut pending, &mut book, &events, &mut warnings);
            }
        }
        if ev.kind == Kind::Split {
            pending.entry(ev.symbol.clone()).or_default().push(idx);
            continue;
        }
        if ev.kind.is_position() {
            let kind: &'static str = ev.kind.name();
            // close existing opposite lots FIFO
            let mut q = ev.qty;
            {
                let dq = book.get(&ev.symbol);
                while q.abs() > EPS && !dq.is_empty() && (dq[0].qty > 0.0) != (q > 0.0) {
                    let lot = &mut dq[0];
                    let take = q.abs().min(lot.qty.abs());
                    let close_fee = if ev.qty != 0.0 { ev.fee * take / ev.qty.abs() } else { 0.0 };
                    closed.push(ClosedTrade {
                        symbol: ev.symbol.clone(),
                        underlying: ev.underlying.clone(),
                        is_option: ev.is_option,
                        long: lot.qty > 0.0,
                        qty: take,
                        open_time: lot.time,
                        close_time: ev.time,
                        open_price: lot.price,
                        close_price: ev.price,
                        multiplier: lot.multiplier,
                        close_kind: kind,
                        combo_id: lot.combo_id,
                        origin: lot.origin,
                        fees: lot.fee_per_unit * take + close_fee,
                        open_key: if lot.order_key.is_empty() { iso_key(&lot.time) } else { lot.order_key.clone() },
                        close_key: ev.order_id.clone().unwrap_or_else(|| iso_key(&ev.time)),
                    });
                    if lot.qty < 0.0 {
                        lot.qty += take;
                    } else {
                        lot.qty -= take;
                    }
                    if q < 0.0 {
                        q += take;
                    } else {
                        q -= take;
                    }
                    if lot.qty.abs() < EPS {
                        dq.pop_front();
                    }
                }
            }
            if q.abs() > EPS {
                if matches!(ev.kind, Kind::Expired | Kind::Assigned | Kind::Exercised) {
                    warnings.push(format!(
                        "{} {} {} qty {}: no matching open position (history may start mid-position)",
                        ev.time.format("%Y-%m-%d"),
                        ev.raw_type,
                        ev.symbol,
                        plus_g(ev.qty)
                    ));
                    continue;
                }
                let combo_id = if ev.is_option {
                    let und = ev.underlying.clone();
                    let start_new = match forming.get(&und) {
                        None => true,
                        Some(&cid) => {
                            let c = &combos[cid - 1];
                            util::secs(&c.last_open_time.unwrap_or(c.open_time), &ev.time) > combo_window_s
                        }
                    };
                    if start_new {
                        let held = book.position(&und);
                        let id = combos.len() + 1;
                        combos.push(Combo {
                            id,
                            underlying: und.clone(),
                            open_time: ev.time,
                            legs: IndexMap::new(),
                            open_cash: 0.0,
                            last_open_time: None,
                            rolled_from: None,
                            rolled_to: None,
                            stock_held_at_open: held,
                            strategy: String::new(),
                        });
                        forming.insert(und.clone(), id);
                    }
                    let cid = forming[&und];
                    let c = &mut combos[cid - 1];
                    *c.legs.entry(ev.symbol.clone()).or_insert(0.0) += q;
                    c.open_cash += -q * ev.price * ev.multiplier;
                    c.last_open_time = Some(ev.time);
                    Some(cid)
                } else {
                    None
                };
                let lot = Lot {
                    qty: q,
                    price: ev.price,
                    time: ev.time,
                    multiplier: ev.multiplier,
                    combo_id,
                    origin: if ev.kind == Kind::Delivery { "assignment" } else { "trade" },
                    fee_per_unit: if ev.qty != 0.0 { ev.fee / ev.qty.abs() } else { 0.0 },
                    order_key: ev.order_id.clone().unwrap_or_else(|| iso_key(&ev.time)),
                };
                book.get(&ev.symbol).push_back(lot);
            }
        } else if ev.kind == Kind::Spin {
            let lot = Lot { qty: ev.qty, price: ev.price, time: ev.time, multiplier: 1.0, combo_id: None, origin: "spinoff", fee_per_unit: 0.0, order_key: String::new() };
            book.get(&ev.symbol).push_back(lot);
            warnings.push(format!(
                "{} spinoff {} x{} booked at {} (basis not transferred from parent)",
                ev.ny_date().format("%Y-%m-%d"),
                ev.symbol,
                g(ev.qty),
                g(ev.price)
            ));
        } else if ev.kind == Kind::TransferIn && ev.qty.abs() > EPS {
            let lot = Lot { qty: ev.qty, price: ev.price, time: ev.time, multiplier: 1.0, combo_id: None, origin: "transfer", fee_per_unit: 0.0, order_key: String::new() };
            book.get(&ev.symbol).push_back(lot);
        }
    }
    let keys: Vec<String> = pending.keys().cloned().collect();
    for s in keys {
        flush_split(&s, &mut pending, &mut book, &events, &mut warnings);
    }

    // roll detection: a combo opening within the window of a *trade* closing another combo on the same underlying
    for ci in 0..combos.len() {
        let (cid, und, ot) = (combos[ci].id, combos[ci].underlying.clone(), combos[ci].open_time);
        let mut best: Option<(f64, usize)> = None;
        for ct in &closed {
            let Some(other) = ct.combo_id else { continue };
            if other == cid || ct.close_kind != "trade" || ct.underlying != und || other > cid {
                continue;
            }
            let gap = util::secs(&ct.close_time, &ot).abs();
            if gap <= combo_window_s && best.map(|b| gap < b.0).unwrap_or(true) {
                best = Some((gap, other));
            }
        }
        if let Some((_, from)) = best {
            combos[ci].rolled_from = Some(from);
            if combos[from - 1].rolled_to.is_none() {
                combos[from - 1].rolled_to = Some(cid);
            }
        }
    }
    for c in combos.iter_mut() {
        c.strategy = classify(c);
    }
    let open_lots: IndexMap<String, Vec<Lot>> =
        book.lots.into_iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k, v.into_iter().collect())).collect();
    Ledger { events, closed, open_lots, combos, warnings }
}

/// datetime.isoformat() of a UTC instant ('2026-01-05T15:00:00+00:00').
fn iso_key(t: &Ts) -> String {
    let n = t.naive_utc();
    if n.and_utc().timestamp_subsec_micros() != 0 {
        n.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    } else {
        n.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    }
}

/// f"{x:+g}"
pub fn plus_g(x: f64) -> String {
    let s = g(x);
    if s.starts_with('-') {
        s
    } else {
        format!("+{}", s)
    }
}

// --------------------------------------------------------------------------------- classify
pub fn classify(c: &Combo) -> String {
    let legs: Vec<(Contract, f64)> = c
        .legs
        .iter()
        .filter(|(_, q)| q.abs() > EPS)
        .filter_map(|(s, q)| parse_occ(s).map(|k| (k, *q)))
        .collect();
    if legs.is_empty() {
        return "Unknown".into();
    }
    let n = legs.len();
    if n == 1 {
        let (k, q) = &legs[0];
        if *q > 0.0 {
            return if k.is_call() { "Long call" } else { "Long put" }.into();
        }
        if k.is_call() {
            return if c.stock_held_at_open >= 100.0 * q.abs() - EPS { "Covered call" } else { "Naked call" }.into();
        }
        return "Short put".into();
    }
    let calls: Vec<&(Contract, f64)> = legs.iter().filter(|(k, _)| k.is_call()).collect();
    let puts: Vec<&(Contract, f64)> = legs.iter().filter(|(k, _)| !k.is_call()).collect();
    let same_exp = legs.iter().all(|(k, _)| k.expiry == legs[0].0.expiry);
    let mut qs: Vec<f64> = Vec::new();
    for (_, q) in &legs {
        if !qs.contains(&q.abs()) {
            qs.push(q.abs());
        }
    }

    if n == 2 {
        let ((k1, q1), (k2, q2)) = (&legs[0], &legs[1]);
        if k1.right == k2.right && (*q1 > 0.0) != (*q2 > 0.0) {
            let (long_k, short_k) = if *q1 > 0.0 { (k1, k2) } else { (k2, k1) };
            if k1.expiry != k2.expiry {
                if long_k.strike == short_k.strike {
                    return if long_k.expiry > short_k.expiry { "Calendar spread" } else { "Reverse calendar" }.into();
                }
                return "Diagonal spread".into();
            }
            if qs.len() > 1 {
                return "Ratio spread".into();
            }
            if long_k.is_call() {
                return if long_k.strike < short_k.strike { "Bull call spread" } else { "Bear call spread" }.into();
            }
            return if long_k.strike > short_k.strike { "Bear put spread" } else { "Bull put spread" }.into();
        }
        if k1.right == k2.right {
            let what = if k1.strike != k2.strike { "2 strikes" } else { "2 expiries" };
            return format!("{} {}s ({})", if *q1 > 0.0 { "Long" } else { "Short" }, if k1.is_call() { "call" } else { "put" }, what);
        }
        let cq = calls[0].1;
        let pq = puts[0].1;
        if (cq > 0.0) == (pq > 0.0) {
            let side = if cq > 0.0 { "Long" } else { "Short" };
            return if calls[0].0.strike == puts[0].0.strike && same_exp { format!("{} straddle", side) } else { format!("{} strangle", side) };
        }
        return if cq > 0.0 { "Risk reversal (synthetic long)" } else { "Risk reversal (synthetic short)" }.into();
    }

    if n == 3 && same_exp && (calls.len() == 3 || puts.len() == 3) {
        let mut srt: Vec<&(Contract, f64)> = legs.iter().collect();
        srt.sort_by(|a, b| a.0.strike.partial_cmp(&b.0.strike).unwrap());
        if srt[0].1 > 0.0 && srt[2].1 > 0.0 && srt[1].1 < 0.0 && srt[1].1.abs() == 2.0 * srt[0].1.abs() {
            return "Long butterfly".into();
        }
        if srt[0].1 < 0.0 && srt[2].1 < 0.0 && srt[1].1 > 0.0 {
            return "Short butterfly".into();
        }
    }

    if n == 4 && same_exp && calls.len() == 2 && puts.len() == 2 && qs.len() == 1 {
        let vert = |pair: &[&(Contract, f64)], is_call: bool| -> Option<(&'static str, f64)> {
            let ((a, qa), (b, qb)) = (pair[0], pair[1]);
            if (*qa > 0.0) == (*qb > 0.0) {
                return None;
            }
            let (lng, sht) = if *qa > 0.0 { (a, b) } else { (b, a) };
            let debit = if is_call { lng.strike < sht.strike } else { lng.strike > sht.strike };
            Some((if debit { "debit" } else { "credit" }, sht.strike))
        };
        if let (Some(vc), Some(vp)) = (vert(&calls, true), vert(&puts, false)) {
            if vc.0 == "credit" && vp.0 == "credit" {
                // short call below the short put: the strikes cross ("inverted"), so the credit covers
                // part of a loss that is certain at expiration
                return if vc.1 == vp.1 { "Iron butterfly" } else if vc.1 < vp.1 { "Inverted iron condor" } else { "Iron condor" }.into();
            }
            if vc.0 == "debit" && vp.0 == "debit" {
                return "Reverse iron condor".into();
            }
            let bias = if vc.0 == "debit" { "Bull" } else { "Bear" };
            return format!("{}ish double vertical", bias);
        }
    }
    let side = if legs.iter().all(|(_, q)| *q > 0.0) {
        "long"
    } else if legs.iter().all(|(_, q)| *q < 0.0) {
        "short"
    } else {
        "mixed"
    };
    format!("Custom {}-leg ({})", n, side)
}

/// Best-effort defined risk ($) at entry; NaN when undefined.
pub fn max_risk(c: &Combo) -> f64 {
    let legs: Vec<(Option<Contract>, f64)> = c.legs.iter().filter(|(_, q)| q.abs() > EPS).map(|(s, q)| (parse_occ(s), *q)).collect();
    let debit = -c.open_cash;
    if legs.iter().all(|(_, q)| *q > 0.0) {
        return debit;
    }
    let s = c.strategy.as_str();
    let strike = |i: usize| legs[i].0.as_ref().map(|k| k.strike).unwrap_or(f64::NAN);
    if matches!(s, "Bull call spread" | "Bear call spread" | "Bear put spread" | "Bull put spread") {
        let width = (strike(0) - strike(1)).abs() * legs[0].1.abs() * 100.0;
        return if debit > 0.0 { debit } else { width - c.open_cash };
    }
    if matches!(s, "Iron condor" | "Inverted iron condor" | "Iron butterfly" | "Reverse iron condor") || s.ends_with("double vertical") {
        let mut calls: Vec<f64> = legs.iter().filter_map(|(k, _)| k.as_ref().filter(|k| k.is_call()).map(|k| k.strike)).collect();
        let mut puts: Vec<f64> = legs.iter().filter_map(|(k, _)| k.as_ref().filter(|k| !k.is_call()).map(|k| k.strike)).collect();
        calls.sort_by(|a, b| a.partial_cmp(b).unwrap());
        puts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let width = (calls[1] - calls[0]).max(puts[1] - puts[0]) * legs[0].1.abs() * 100.0;
        return if debit > 0.0 { debit } else { width - c.open_cash };
    }
    if matches!(s, "Calendar spread" | "Diagonal spread" | "Long butterfly") && debit > 0.0 {
        return debit;
    }
    f64::NAN
}
