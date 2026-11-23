//! Open (unfilled) orders: what each opening option order would do if it filled.

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{json, Value};

use crate::interpret::{evaluate as eval_group, grid, name_group, Greeks, Unit, STOCK};
use crate::jv;
use crate::ledger::{classify, Combo, Ledger};
use crate::occ::{parse_occ, underlying_of};
use crate::optmath::{bs_price, payoff_curves, LegIn};
use crate::reprice::{self as rp, LegModel, Market};
use crate::util::{self, erf, linspace, round_dp, Ts};

use crate::analysis::OpenPos;

const DONE: [&str; 9] = ["filled", "canceled", "expired", "replaced", "rejected", "stopped", "suspended", "done_for_day", "pending_cancel"];

#[derive(Clone, Debug)]
pub struct OrderLeg {
    pub symbol: String,
    pub ratio: f64,
    pub intent: String,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub iv: Option<f64>,
    pub greeks: Greeks,
    pub model: Option<LegModel>,
}

impl OrderLeg {
    pub fn is_option(&self) -> bool {
        parse_occ(&self.symbol).is_some()
    }
    pub fn mid(&self) -> Option<f64> {
        let t = |x: Option<f64>| x.filter(|v| *v != 0.0);
        match (t(self.bid), t(self.ask)) {
            (Some(b), Some(a)) => Some((b + a) / 2.0),
            (b, a) => a.or(b),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OrderCurves {
    pub x: Vec<f64>,
    pub expiry: Vec<f64>,
    pub today: Vec<f64>,
    pub position: Option<Vec<f64>>,
    pub combined: Option<Vec<f64>>,
    pub combined_today: Option<Vec<f64>>,
    pub pos_max_loss: f64,
    pub pos_max_gain: f64,
    pub comb_max_loss: f64,
    pub comb_max_gain: f64,
    pub comb_breakevens: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct ModelPayload {
    pub legs: Vec<[f64; 5]>,
    pub shares: f64,
    pub cost: f64,
}

impl ModelPayload {
    pub fn to_json(&self) -> Value {
        json!({"legs": self.legs.iter().map(|l| json!([l[0], l[1] as i64, l[2], l[3], l[4]])).collect::<Vec<_>>(),
               "shares": self.shares, "cost": self.cost})
    }
}

#[derive(Clone, Debug)]
pub struct OrderModel {
    pub order: ModelPayload,
    pub pos: Option<ModelPayload>,
}

#[derive(Clone, Debug)]
pub struct PendingOrder {
    pub id: String,
    pub submitted: Option<Ts>,
    pub status: String,
    pub order_type: String,
    pub tif: String,
    pub units: f64,
    pub legs: Vec<OrderLeg>,
    pub net: Option<f64>,
    pub net_is_estimate: bool,
    pub stop: Option<f64>,
    pub attached: String,
    pub underlying: String,
    pub is_option: bool,
    pub effect: String,
    pub strategy: String,
    pub spot: Option<f64>,
    pub mid_now: Option<f64>,
    pub natural: Option<f64>,
    pub max_gain: f64,
    pub max_loss: f64,
    pub breakevens: Vec<f64>,
    pub pop: Option<f64>,
    pub expiry: Option<NaiveDate>,
    pub curves: Option<OrderCurves>,
    pub est_open: Option<f64>,
    pub model: Option<OrderModel>,
    pub fill: Option<crate::fillodds::FillOdds>,
    /// none of the option legs has a quote, a trade or an implied volatility yet (e.g. newly listed
    /// contracts before their first session): nothing to estimate prices from
    pub no_market: bool,
}

impl PendingOrder {
    pub fn mult(&self) -> f64 {
        if self.is_option {
            100.0
        } else {
            1.0
        }
    }
    pub fn legs_label(&self) -> String {
        self.legs
            .iter()
            .map(|l| {
                let q = l.ratio * self.units;
                let what = match parse_occ(&l.symbol) {
                    Some(c) => format!("{} {}{}", c.expiry.format("%m/%d"), util::g(c.strike), c.right),
                    None => format!("{} sh", l.symbol),
                };
                format!("{}{} {}", if q > 0.0 { "+" } else { "−" }, util::g(q.abs()), what)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn intent(pi: &str) -> String {
    if pi.ends_with("_to_open") {
        "open".into()
    } else if pi.ends_with("_to_close") {
        "close".into()
    } else {
        String::new()
    }
}

pub fn parse_orders(raw: &[Value]) -> Vec<PendingOrder> {
    let mut out = Vec::new();
    for o in raw {
        let status = jv::s(o, "status");
        if DONE.contains(&status.as_str()) {
            continue;
        }
        let qty = jv::ff(o, "qty").unwrap_or(0.0);
        let units = qty - jv::ff(o, "filled_qty").unwrap_or(0.0);
        if units <= 0.0 {
            continue;
        }
        let typ = jv::opt_s(o, "type").or_else(|| jv::opt_s(o, "order_type")).unwrap_or_default();
        let lim = jv::ff(o, "limit_price");
        let stop = jv::ff(o, "stop_price");
        let sub = jv::opt_s(o, "submitted_at").or_else(|| jv::opt_s(o, "created_at"));
        let mk = |legs: Vec<OrderLeg>, net: Option<f64>, attached: String| PendingOrder {
            id: jv::s(o, "id"),
            submitted: sub.as_deref().and_then(rp::ts),
            status: status.clone(),
            order_type: typ.clone(),
            tif: jv::s(o, "time_in_force"),
            units,
            legs,
            net,
            net_is_estimate: false,
            stop,
            attached,
            underlying: String::new(),
            is_option: false,
            effect: "open".into(),
            strategy: String::new(),
            spot: None,
            mid_now: None,
            natural: None,
            max_gain: f64::NAN,
            max_loss: f64::NAN,
            breakevens: vec![],
            pop: None,
            expiry: None,
            curves: None,
            est_open: None,
            model: None,
            fill: None,
            no_market: false,
        };
        let leg = |sym: String, ratio: f64, it: String| OrderLeg { symbol: sym, ratio, intent: it, bid: None, ask: None, iv: None, greeks: Greeks::default(), model: None };
        if jv::s(o, "order_class") == "mleg" {
            let legs: Vec<OrderLeg> = jv::arr(o.get("legs"))
                .iter()
                .filter(|l| !jv::s(l, "symbol").is_empty())
                .map(|l| {
                    let r = jv::ff(l, "ratio_qty").filter(|v| *v != 0.0).unwrap_or(1.0);
                    leg(jv::s(l, "symbol"), r * if jv::s(l, "side") == "buy" { 1.0 } else { -1.0 }, intent(&jv::s(l, "position_intent")))
                })
                .collect();
            if legs.is_empty() {
                continue;
            }
            out.push(mk(legs, lim, String::new()));
            continue;
        }
        let sym = jv::s(o, "symbol");
        if sym.is_empty() {
            continue;
        }
        let sgn = if jv::s(o, "side") == "buy" { 1.0 } else { -1.0 };
        let price = lim.or(if typ == "stop" { stop } else { None });
        let mut children = Vec::new();
        for l in jv::arr(o.get("legs")) {
            let desc = jv::s(l, "type");
            let p = jv::ff(l, "limit_price").filter(|v| *v != 0.0).or_else(|| jv::ff(l, "stop_price"));
            if let Some(p) = p {
                children.push(format!("{} {} ${}", jv::s(l, "side"), desc, util::commas(p, 2)).trim().to_string());
            }
        }
        out.push(mk(vec![leg(sym, sgn, intent(&jv::s(o, "position_intent")))], price.map(|p| sgn * p), children.join("; ")));
    }
    for po in out.iter_mut() {
        po.underlying = underlying_of(&po.legs[0].symbol);
        po.is_option = po.legs.iter().any(|l| l.is_option());
    }
    out
}

// --------------------------------------------------------------------------------- evaluation
fn pop(legs: &[LegIn], spot: f64, today: NaiveDate, expiry: Option<NaiveDate>, iv: Option<f64>) -> Option<f64> {
    let iv = iv.filter(|v| *v != 0.0 && !v.is_nan())?;
    let expiry = expiry?;
    if spot == 0.0 {
        return None;
    }
    let t = ((expiry - today).num_days().max(0)) as f64 / 365.0 + 0.5 / 365.0;
    let s = iv * t.sqrt();
    let xs: Vec<f64> = linspace(-6.0 * s, 6.0 * s, 2401).iter().map(|z| spot * z.exp()).collect();
    let y = payoff_curves(legs, spot, today, 0.0, 0.0, &xs).expiry;
    let cdf: Vec<f64> = xs.iter().map(|x| ((x / spot).ln() + 0.5 * s * s) / s).map(|z| 0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))).collect();
    let mut p = 0.0;
    for i in 0..xs.len() - 1 {
        if (y[i] + y[i + 1]) / 2.0 > 0.0 {
            p += cdf[i + 1] - cdf[i];
        }
    }
    Some(p)
}

pub fn order_units(po: &PendingOrder) -> Vec<Unit> {
    let mult = po.mult();
    let mut us: Vec<Unit> = po
        .legs
        .iter()
        .map(|l| {
            let q = l.ratio * po.units;
            let m = l.mid().unwrap_or(0.0);
            Unit {
                symbol: if l.is_option() { l.symbol.clone() } else { STOCK.into() },
                qty: q,
                cost: q * mult * m,
                value: q * mult * m,
                greeks: l.greeks.clone(),
                iv: l.iv,
            }
        })
        .collect();
    if let Some(net) = po.net {
        let gap = net * po.units * mult - us.iter().map(|u| u.cost).sum::<f64>();
        let n = us.len() as f64;
        for u in us.iter_mut() {
            u.cost += gap / n;
        }
    }
    us
}

pub fn position_units(open_positions: &[OpenPos], und: &str, snapshots: &Value, today: NaiveDate) -> Vec<Unit> {
    let mut us = Vec::new();
    for r in open_positions {
        let mv = if r.market_value.is_nan() { 0.0 } else { r.market_value };
        if r.is_option && underlying_of(&r.symbol) == und && parse_occ(&r.symbol).map(|c| c.expiry >= today).unwrap_or(false) {
            let snap = snapshots.get(&r.symbol).unwrap_or(jv::empty_obj());
            us.push(Unit { symbol: r.symbol.clone(), qty: r.qty, cost: r.cost_basis, value: mv, greeks: Greeks::default(), iv: jv::f(snap, "impliedVolatility") });
        } else if !r.is_option && r.symbol == und {
            let q = r.qty;
            let lot_q = (100.0 * (q.abs() / 100.0).floor()).copysign(q);
            if lot_q != 0.0 {
                let f = lot_q / q;
                us.push(Unit { symbol: STOCK.into(), qty: lot_q, cost: r.cost_basis * f, value: mv * f, greeks: Greeks::default(), iv: None });
            }
        }
    }
    us
}

fn order_name(units: &[&Unit]) -> String {
    let n = name_group(units);
    if !n.is_empty() {
        return n;
    }
    let mut legs: indexmap::IndexMap<String, f64> = indexmap::IndexMap::new();
    for u in units {
        if u.symbol != STOCK {
            *legs.entry(u.symbol.clone()).or_insert(0.0) += u.qty;
        }
    }
    legs.retain(|_, v| v.abs() > 1e-9);
    if legs.is_empty() {
        return "Stock".into();
    }
    classify(&Combo::bare(legs))
}

pub fn models_for(units: &[&Unit], snapshots: &Value, market: Option<&Market>, und: &str, ivs: &HashMap<String, f64>) -> HashMap<String, LegModel> {
    let mut out = HashMap::new();
    let Some(market) = market else { return out };
    let Some(u) = market.underlyings.get(und) else { return out };
    for un in units {
        if un.symbol != STOCK && !out.contains_key(&un.symbol) {
            let snap = snapshots.get(&un.symbol).unwrap_or(jv::empty_obj());
            if let Some(m) = rp::calibrate(&un.symbol, snap, u, market, ivs.get(und).copied()) {
                out.insert(un.symbol.clone(), m);
            }
        }
    }
    out
}

pub fn curve_open(units: &[&Unit], models: &HashMap<String, LegModel>, xs: &[f64], iv_shift: f64) -> Vec<f64> {
    let base = -units.iter().map(|u| u.cost).sum::<f64>();
    xs.iter()
        .map(|&x| {
            let mut y = base;
            for u in units {
                if u.symbol == STOCK {
                    y += u.qty * x;
                } else {
                    let m = &models[&u.symbol];
                    y += 100.0 * u.qty * bs_price(x, m.strike, m.t, (m.iv * (1.0 + iv_shift)).max(1e-3), m.right, rp::R);
                }
            }
            round_dp(y, 2)
        })
        .collect()
}

pub fn model_payload(units: &[&Unit], models: &HashMap<String, LegModel>) -> ModelPayload {
    let mut legs = Vec::new();
    let mut shares = 0.0;
    for u in units {
        if u.symbol == STOCK {
            shares += u.qty;
        } else {
            let m = &models[&u.symbol];
            legs.push([m.strike, if m.right == 'C' { 1.0 } else { 0.0 }, round_dp(100.0 * u.qty, 6), round_dp(m.t, 8), round_dp(m.iv, 6)]);
        }
    }
    ModelPayload { legs, shares, cost: round_dp(units.iter().map(|u| u.cost).sum(), 4) }
}

fn legs_in(units: &[&Unit]) -> Vec<LegIn> {
    units.iter().filter(|u| u.symbol != STOCK).map(|u| LegIn { symbol: u.symbol.clone(), qty: u.qty, cost: u.cost, iv: u.iv }).collect()
}

pub fn evaluate(
    orders: &mut [PendingOrder],
    ledger: &Ledger,
    open_positions: &[OpenPos],
    snapshots: &Value,
    spot_of: &dyn Fn(&str) -> Option<f64>,
    today: NaiveDate,
    market: Option<&Market>,
) {
    let und_ivs = rp::underlying_ivs(snapshots);
    for po in orders.iter_mut() {
        let units_n = po.units;
        for l in po.legs.iter_mut() {
            let snap = snapshots.get(&l.symbol).unwrap_or(jv::empty_obj());
            let q = snap.get("latestQuote").unwrap_or(jv::empty_obj());
            l.bid = jv::ff(q, "bp").filter(|v| *v != 0.0);
            l.ask = jv::ff(q, "ap").filter(|v| *v != 0.0);
            if l.bid.is_none() && l.ask.is_none() {
                let t = snap.get("latestTrade").and_then(|t| jv::ff(t, "p"));
                l.bid = t;
                l.ask = t;
            }
            l.iv = jv::ff(snap, "impliedVolatility");
            let m = l.ratio * units_n * if l.is_option() { 100.0 } else { 1.0 };
            l.greeks = match jv::obj(snap.get("greeks")).filter(|o| !o.is_empty()) {
                Some(g) => {
                    let v = |k: &str| g.get(k).and_then(jv::float_of).filter(|x| x.is_finite() && *x != 0.0).unwrap_or(0.0) * m;
                    Greeks { delta: Some(v("delta")), theta: Some(v("theta")), vega: Some(v("vega")) }
                }
                None if !l.is_option() => Greeks { delta: Some(m), ..Default::default() },
                None => Greeks::default(),
            };
        }
        po.no_market = po.is_option && po.legs.iter().filter(|l| l.is_option()).all(|l| l.bid.is_none() && l.ask.is_none() && l.iv.is_none());
        let mu = market.and_then(|m| m.underlyings.get(&po.underlying));
        po.spot = mu.and_then(|u| u.spot).filter(|v| *v != 0.0).or_else(|| spot_of(&po.underlying));
        let mids: Vec<Option<f64>> = po.legs.iter().map(|l| l.mid()).collect();
        if mids.iter().all(|m| m.is_some()) {
            po.mid_now = Some(po.legs.iter().zip(&mids).map(|(l, m)| l.ratio * m.unwrap()).sum());
        }
        let nat = |l: &OrderLeg| if l.ratio > 0.0 { l.ask } else { l.bid };
        if po.legs.iter().all(|l| nat(l).map(|v| v != 0.0).unwrap_or(false)) {
            po.natural = Some(po.legs.iter().map(|l| l.ratio * nat(l).unwrap()).sum());
        }
        if po.net.is_none() && po.mid_now.is_some() {
            po.net = po.mid_now;
            po.net_is_estimate = true;
        }
        let mut effects: Vec<String> = Vec::new();
        for l in &po.legs {
            let e = if !l.intent.is_empty() {
                l.intent.clone()
            } else {
                let pos: f64 = ledger.open_lots.get(&l.symbol).map(|v| v.iter().map(|x| x.qty).sum()).unwrap_or(0.0);
                if pos != 0.0 && (pos > 0.0) != (l.ratio > 0.0) { "close".into() } else { "open".into() }
            };
            if !effects.contains(&e) {
                effects.push(e);
            }
        }
        po.effect = if effects.len() == 1 { effects[0].clone() } else { "mixed".into() };
        if !po.is_option || po.effect == "close" {
            continue;
        }
        let units = order_units(po);
        let ur: Vec<&Unit> = units.iter().collect();
        po.strategy = order_name(&ur);
        po.expiry = po.legs.iter().filter_map(|l| parse_occ(&l.symbol).map(|c| c.expiry)).min();
        let (Some(spot), Some(_)) = (po.spot.filter(|s| *s != 0.0), po.net) else { continue };
        let pos = position_units(open_positions, &po.underlying, snapshots, today);
        let allu: Vec<&Unit> = units.iter().chain(pos.iter()).collect();
        let (xs, risk_xs) = grid(&allu, spot);
        let g = eval_group(&ur, &po.strategy, spot, today, &xs, &risk_xs);
        po.max_gain = g.max_gain;
        po.max_loss = g.max_loss;
        po.breakevens = g.breakevens.clone();
        let mut ivs: Vec<f64> = po
            .legs
            .iter()
            .filter(|l| l.iv.map(|v| v != 0.0).unwrap_or(false) && l.is_option() && parse_occ(&l.symbol).map(|c| Some(c.expiry) == po.expiry).unwrap_or(false))
            .map(|l| l.iv.unwrap())
            .collect();
        if ivs.is_empty() {
            let mut near: Vec<(f64, NaiveDate)> = Vec::new();
            if let Some(o) = jv::obj(Some(snapshots)) {
                for (k, v) in o {
                    if let Some(c) = parse_occ(k) {
                        if c.underlying == po.underlying {
                            if let Some(iv) = jv::ff(v, "impliedVolatility").filter(|x| *x != 0.0) {
                                near.push((iv, c.expiry));
                            }
                        }
                    }
                }
            }
            ivs = near.iter().filter(|(_, e)| Some(*e) == po.expiry).map(|x| x.0).collect();
            if ivs.is_empty() {
                ivs = near.iter().map(|x| x.0).collect();
            }
        }
        let iv = if ivs.is_empty() { None } else { Some(util::mean(&ivs)) };
        po.pop = pop(&legs_in(&ur), spot, today, po.expiry, iv);
        let mut curves = OrderCurves { x: g.curve_x.clone(), expiry: g.curve_expiry.clone(), today: g.curve_today.clone(), ..Default::default() };
        let mut models = HashMap::new();
        if let Some(m) = market.filter(|m| m.underlyings.contains_key(&po.underlying) && !po.no_market) {
            models = models_for(&allu, snapshots, Some(m), &po.underlying, &und_ivs);
            for l in po.legs.iter_mut() {
                l.model = models.get(&l.symbol).cloned();
            }
            curves.today = curve_open(&ur, &models, &g.curve_x, 0.0);
            po.est_open = Some(po.legs.iter().filter_map(|l| l.model.as_ref().map(|m| l.ratio * m.value(spot, 0.0))).sum());
            let pr: Vec<&Unit> = pos.iter().collect();
            po.model = Some(OrderModel { order: model_payload(&ur, &models), pos: if pos.is_empty() { None } else { Some(model_payload(&pr, &models)) } });
        }
        if !pos.is_empty() {
            let pr: Vec<&Unit> = pos.iter().collect();
            let cur = eval_group(&pr, "Position", spot, today, &xs, &risk_xs);
            let mut comb = eval_group(&allu, "Position + order", spot, today, &xs, &risk_xs);
            if po.model.is_some() {
                comb.curve_today = curve_open(&allu, &models, &g.curve_x, 0.0);
            }
            curves.position = Some(cur.curve_expiry);
            curves.combined = Some(comb.curve_expiry);
            curves.combined_today = Some(comb.curve_today);
            curves.pos_max_loss = cur.max_loss;
            curves.pos_max_gain = cur.max_gain;
            curves.comb_max_loss = comb.max_loss;
            curves.comb_max_gain = comb.max_gain;
            curves.comb_breakevens = comb.breakevens;
        }
        po.curves = Some(curves);
    }
}

pub struct Combined {
    pub curves: OrderCurves,
    pub max_gain: f64,
    pub max_loss: f64,
    pub breakevens: Vec<f64>,
    pub net: f64,
    pub spot: f64,
    pub n: usize,
    pub est_open: Option<f64>,
    pub model: Option<OrderModel>,
}

/// Every order on one underlying filling together (plus the current position).
pub fn combine(orders: &[&PendingOrder], open_positions: &[OpenPos], snapshots: &Value, today: NaiveDate, market: Option<&Market>) -> Option<Combined> {
    let os: Vec<&&PendingOrder> = orders.iter().filter(|o| o.is_option && o.net.is_some() && o.spot.map(|s| s != 0.0).unwrap_or(false)).collect();
    if os.len() < 2 {
        return None;
    }
    let spot = os[0].spot.unwrap();
    let units: Vec<Unit> = os.iter().flat_map(|o| order_units(o)).collect();
    let pos = position_units(open_positions, &os[0].underlying, snapshots, today);
    let ur: Vec<&Unit> = units.iter().collect();
    let allu: Vec<&Unit> = units.iter().chain(pos.iter()).collect();
    let (xs, risk_xs) = grid(&allu, spot);
    let g = eval_group(&ur, "All orders", spot, today, &xs, &risk_xs);
    let mut d = Combined {
        curves: OrderCurves { x: g.curve_x.clone(), expiry: g.curve_expiry.clone(), today: g.curve_today.clone(), ..Default::default() },
        max_gain: g.max_gain,
        max_loss: g.max_loss,
        breakevens: g.breakevens.clone(),
        net: os.iter().map(|o| o.net.unwrap() * o.units * o.mult()).sum(),
        spot,
        n: os.len(),
        est_open: None,
        model: None,
    };
    let mut models = HashMap::new();
    if let Some(m) = market.filter(|m| m.underlyings.contains_key(&os[0].underlying) && !os.iter().any(|o| o.no_market)) {
        models = models_for(&allu, snapshots, Some(m), &os[0].underlying, &rp::underlying_ivs(snapshots));
        d.curves.today = curve_open(&ur, &models, &g.curve_x, 0.0);
        d.est_open = Some(os.iter().filter_map(|o| o.est_open.map(|e| e * o.units * o.mult())).sum());
        let pr: Vec<&Unit> = pos.iter().collect();
        d.model = Some(OrderModel { order: model_payload(&ur, &models), pos: if pos.is_empty() { None } else { Some(model_payload(&pr, &models)) } });
    }
    if !pos.is_empty() {
        let pr: Vec<&Unit> = pos.iter().collect();
        let cur = eval_group(&pr, "Position", spot, today, &xs, &risk_xs);
        let mut comb = eval_group(&allu, "Position + orders", spot, today, &xs, &risk_xs);
        if !models.is_empty() {
            comb.curve_today = curve_open(&allu, &models, &g.curve_x, 0.0);
        }
        d.curves.position = Some(cur.curve_expiry);
        d.curves.combined = Some(comb.curve_expiry);
        d.curves.combined_today = Some(comb.curve_today);
        d.curves.comb_max_loss = comb.max_loss;
        d.curves.comb_max_gain = comb.max_gain;
        d.curves.comb_breakevens = comb.breakevens;
    }
    Some(d)
}
