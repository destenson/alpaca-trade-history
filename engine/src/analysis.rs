//! Assemble every table the report needs from a bundle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{Datelike, NaiveDate};
use indexmap::IndexMap;
use serde_json::Value;

use crate::jv;
use crate::ledger::{allocate_fees, build_ledger, max_risk, normalize, Kind, Ledger, Trade};
use crate::metrics::{self as m, BenchFrame, Drawdown, EqFrame, Month, PerfStats, Rolling, StatInput};
use crate::occ::{parse_occ, underlying_of};
use crate::util::{self, cummax, ny_date, Ts, NAN};

#[derive(Clone, Debug, Default)]
pub struct Bundle {
    pub activities: Vec<Value>,
    pub portfolio_history: Option<Value>,
    pub positions: Vec<Value>,
    pub account: Option<Value>,
    pub bars: Value,
    /// recent 5-minute closes for underlyings whose option legs have no quotes yet
    pub intraday: Value,
    pub prices: IndexMap<String, f64>,
    pub option_snapshots: Value,
    pub orders: Vec<Value>,
    pub stock_snapshots: Value,
    pub clock: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct OpenPos {
    pub symbol: String,
    pub underlying: String,
    pub is_option: bool,
    pub qty: f64,
    pub avg_price: f64,
    pub price: f64,
    pub broker_avg_price: f64,
    pub market_value: f64,
    /// Alpaca's own market value (options: often the closing side of the quote, see `open_positions`)
    pub broker_value: f64,
    pub cost_basis: f64,
    pub unrealized_pl: f64,
    pub unrealized_pct: f64,
    pub multiplier: f64,
    pub expiry: Option<NaiveDate>,
}

#[derive(Clone, Debug)]
pub struct ComboRow {
    pub combo_id: usize,
    pub underlying: String,
    pub strategy: String,
    pub open_time: Ts,
    pub close_time: Option<Ts>,
    pub n_legs: usize,
    pub legs: String,
    pub net_premium: f64,
    pub premium_side: &'static str,
    pub max_risk: f64,
    pub dte: i64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub pnl: f64,
    pub outcome: &'static str,
    pub is_open: bool,
    pub rolled_from: Option<usize>,
    pub rolled_to: Option<usize>,
    pub underlying_px: f64,
    pub moneyness: f64,
    pub rights: String,
    pub hold_days: f64,
    pub ror: f64,
    pub campaign: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SymRow {
    pub underlying: String,
    pub stock_pnl: f64,
    pub option_pnl: f64,
    pub trades: f64,
    pub wins: f64,
    pub unrealized: f64,
    pub dividends: f64,
    pub total_pnl: f64,
    pub realized: f64,
    pub is_open: bool,
    pub bh_pnl: Option<f64>,
    pub bhc_end: Option<NaiveDate>,
    pub bhc_return: Option<f64>,
    pub bhc_capital: Option<f64>,
    pub bhc_pnl: Option<f64>,
    pub edge_closed: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct CashSummary {
    pub fees: f64,
    pub fees_in_trades: f64,
    pub income: f64,
    pub interest: f64,
    pub flows: f64,
    pub unrealized: f64,
    pub income_by_symbol: BTreeMap<String, f64>,
}

#[derive(Clone, Debug)]
pub struct RecRow {
    pub symbol: String,
    pub ledger_qty: f64,
    pub broker_qty: f64,
    pub diff: f64,
}

#[derive(Clone, Debug)]
pub enum Recon {
    Rows(Vec<RecRow>),
    Matched(bool), // shared view
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub bundle: Bundle,
    pub ledger: Ledger,
    pub trades: Vec<Trade>,
    pub combos: Vec<ComboRow>,
    pub open_positions: Vec<OpenPos>,
    pub symbols: Vec<SymRow>,
    pub equity: Option<EqFrame>,
    pub bench: IndexMap<String, BenchFrame>,
    pub stats: Option<PerfStats>,
    pub bench_stats: IndexMap<String, PerfStats>,
    pub drawdowns: Vec<Drawdown>,
    pub rolling: Option<Rolling>,
    pub monthly: Vec<Month>,
    pub cash_summary: CashSummary,
    pub reconciliation: Option<Recon>,
    pub primary_benchmark: String,
    pub warnings: Vec<String>,
    pub alerts: Vec<String>,
    pub start: Option<NaiveDate>,
    pub share: bool,
    pub notional: f64,
    pub marks: HashMap<String, f64>,
}

pub fn run(bundle: Bundle, benchmarks: &[String], combo_window_s: f64, start: Option<NaiveDate>, rf: f64) -> Analysis {
    let mut events = normalize(&bundle.activities);
    allocate_fees(&mut events, &daily_fee_fallback(&bundle));
    let ledger = build_ledger(events, combo_window_s);
    let mut trades = ledger.closed_df();
    let eq_start = equity_start(&bundle, &ledger);
    let start_d = start.or(eq_start);
    if let Some(sd) = start_d {
        trades.retain(|t| ny_date(&t.close_time) >= sd);
    }
    let marks = marks(&bundle);
    let open_pos = open_positions(&bundle, &ledger, &marks);
    let combos = combos(&ledger, &open_pos, &bundle, start_d);
    let mut cs = cash_summary(&ledger, start_d);
    if cs.fees == 0.0 {
        let fb = daily_fee_fallback(&bundle);
        if !fb.is_empty() {
            cs.fees = fb.values().sum();
        }
    }
    cs.fees_in_trades = ledger.events.iter().filter(|e| e.kind == Kind::Trade).map(|e| e.fee).sum();
    let mut a = Analysis {
        warnings: ledger.warnings.clone(),
        bundle,
        ledger,
        trades,
        combos,
        open_positions: open_pos,
        symbols: vec![],
        equity: None,
        bench: IndexMap::new(),
        stats: None,
        bench_stats: IndexMap::new(),
        drawdowns: vec![],
        rolling: None,
        monthly: vec![],
        cash_summary: cs,
        reconciliation: None,
        primary_benchmark: benchmarks.first().cloned().unwrap_or_else(|| "SPY".into()),
        alerts: vec![],
        start: start_d,
        share: false,
        notional: 10_000.0,
        marks,
    };

    let ph = a.bundle.portfolio_history.clone().filter(|p| jv::truthy(Some(p)));
    match ph.as_ref().map(|p| m::equity_series(p, &a.ledger)) {
        Some(Ok(eq)) => {
            a.alerts.extend(coverage_alerts(&eq, &a.ledger));
            let mut eq = append_live_point(eq, &a.bundle, &a.ledger);
            if let Some(sd) = start_d {
                if let Some(i) = eq.dates.iter().position(|d| *d >= sd) {
                    eq = eq.slice_from(i);
                    eq.flow[0] = eq.equity[0];
                }
            }
            m::add_returns(&mut eq);
            for sym in benchmarks {
                match m::bar_closes(&a.bundle.bars, sym) {
                    Some(c) if c.len() > 2 => {
                        a.bench.insert(sym.clone(), m::benchmark_frame(&eq, &c));
                    }
                    _ => a.warnings.push(format!("No price bars for benchmark {}; skipped.", sym)),
                }
            }
            a.primary_benchmark = a.bench.keys().next().cloned().unwrap_or_else(|| benchmarks.first().cloned().unwrap_or_default());
            let pb = a.bench.get(&a.primary_benchmark);
            a.stats = Some(m::perf_stats(&StatInput::of(&eq), pb, rf));
            let mut bs = IndexMap::new();
            for (sym, bf) in &a.bench {
                bs.insert(sym.clone(), bench_stats(bf, &eq, rf, true));
            }
            a.bench_stats = bs;
            a.drawdowns = m::drawdown_periods(&eq, 5);
            a.rolling = Some(m::rolling_stats(&eq, pb, 21));
            a.monthly = m::monthly(&eq.dates, &eq.ret, Some(&eq.pnl));
            a.equity = Some(eq);
        }
        Some(Err(e)) => a.warnings.push(e),
        None => a.warnings.push("No portfolio history: account-level equity/drawdown charts are disabled.".into()),
    }
    a.symbols = symbol_scorecard(&a);
    a.reconciliation = reconcile(&a.ledger, &a.bundle);
    a
}

/// Score a benchmark frame like the account (equity = benchmark value, same flows).
pub fn bench_stats(bf: &BenchFrame, eq: &EqFrame, rf: f64, subtract_flow: bool) -> PerfStats {
    let dd_dollar: Vec<f64> = {
        let pk = cummax(&bf.cum_pnl);
        bf.cum_pnl.iter().zip(&pk).map(|(c, p)| c - p).collect()
    };
    let _ = subtract_flow;
    let si = StatInput { dates: &eq.dates, ret: &bf.ret, twr: &bf.twr, dd: &bf.dd, dd_dollar: &dd_dollar, flow: &eq.flow, cum_pnl: &bf.cum_pnl, equity: &bf.value };
    m::perf_stats(&si, None, rf)
}

fn marks(b: &Bundle) -> HashMap<String, f64> {
    let mut mk: HashMap<String, f64> = HashMap::new();
    if let Some(o) = b.bars.as_object() {
        for sym in o.keys() {
            if let Some(c) = m::bar_closes(&b.bars, sym) {
                if let Some(l) = c.last() {
                    mk.insert(sym.clone(), l.1);
                }
            }
        }
    }
    for (k, v) in &b.prices {
        mk.insert(k.clone(), *v);
    }
    for p in &b.positions {
        if let Some(px) = jv::f(p, "current_price") {
            mk.insert(jv::s(p, "symbol"), px);
        }
    }
    if let Some(o) = b.option_snapshots.as_object() {
        for (sym, snap) in o {
            let q = snap.get("latestQuote").unwrap_or(jv::empty_obj());
            if let (Some(bp), Some(ap)) = (jv::f(q, "bp").filter(|x| *x != 0.0), jv::f(q, "ap").filter(|x| *x != 0.0)) {
                mk.entry(sym.clone()).or_insert((bp + ap) / 2.0);
            }
        }
    }
    mk
}

fn open_positions(b: &Bundle, ledger: &Ledger, marks: &HashMap<String, f64>) -> Vec<OpenPos> {
    let mut rows = Vec::new();
    if !b.positions.is_empty() {
        for p in &b.positions {
            let sym = jv::s(p, "symbol");
            let c = parse_occ(&sym);
            let qty = jv::f0(p, "qty");
            let mult = if c.is_some() { 100.0 } else { 1.0 };
            // Options are valued at the quote mid: Alpaca's own marks can sit at the closing side of a wide
            // quote (short legs at the ask, long legs at the bid), which can price a spread below what it
            // can possibly be worth. Its figure is kept as `broker_value`.
            let broker_mv = jv::f(p, "market_value").unwrap_or(NAN);
            let mid = c.as_ref().and_then(|_| {
                let q = b.option_snapshots.get(&sym)?.get("latestQuote")?;
                let (bp, ap) = (jv::f(q, "bp").filter(|x| *x > 0.0)?, jv::f(q, "ap").filter(|x| *x > 0.0)?);
                (ap >= bp).then_some((bp + ap) / 2.0)
            });
            let px = mid.or_else(|| jv::f(p, "current_price")).unwrap_or(NAN);
            let mv = match mid {
                Some(m) => qty * m * mult,
                None => broker_mv,
            };
            let broker_cost = jv::f(p, "cost_basis").filter(|x| *x != 0.0).unwrap_or(0.0);
            let lots = ledger.open_lots.get(&sym);
            let cost = match lots {
                Some(l) if !l.is_empty() && (l.iter().map(|x| x.qty).sum::<f64>() - qty).abs() < 1e-6 => l.iter().map(|x| x.qty * x.price).sum::<f64>() * mult,
                _ => broker_cost,
            };
            let upl = if mv.is_nan() { NAN } else { mv - cost };
            rows.push(OpenPos {
                underlying: underlying_of(&sym),
                is_option: c.is_some(),
                qty,
                avg_price: if qty != 0.0 { cost / qty / mult } else { NAN },
                price: px,
                broker_avg_price: jv::f(p, "avg_entry_price").filter(|x| *x != 0.0).unwrap_or(NAN),
                market_value: mv,
                broker_value: broker_mv,
                cost_basis: cost,
                unrealized_pl: upl,
                unrealized_pct: if cost != 0.0 { upl / cost.abs() } else { NAN },
                multiplier: mult,
                expiry: c.map(|c| c.expiry),
                symbol: sym,
            });
        }
    } else {
        for (sym, lots) in &ledger.open_lots {
            let c = parse_occ(sym);
            let mult = if c.is_some() { 100.0 } else { 1.0 };
            let qty: f64 = lots.iter().map(|l| l.qty).sum();
            let cost = lots.iter().map(|l| l.qty * l.price).sum::<f64>() * mult;
            let px = marks.get(sym).copied().unwrap_or(NAN);
            let mv = qty * px * mult;
            rows.push(OpenPos {
                symbol: sym.clone(),
                underlying: underlying_of(sym),
                is_option: c.is_some(),
                qty,
                avg_price: if qty != 0.0 { cost / qty / mult } else { 0.0 },
                price: px,
                broker_avg_price: NAN,
                market_value: mv,
                broker_value: mv,
                cost_basis: cost,
                unrealized_pl: mv - cost,
                unrealized_pct: if cost != 0.0 { (mv - cost) / cost.abs() } else { NAN },
                multiplier: mult,
                expiry: c.map(|c| c.expiry),
            });
        }
    }
    rows
}

pub fn leg_label(sym: &str) -> String {
    match parse_occ(sym) {
        Some(c) => format!("{} {}{}", c.expiry.format("%m/%d"), util::g(c.strike), c.right),
        None => sym.to_string(),
    }
}

fn combos(ledger: &Ledger, open_pos: &[OpenPos], b: &Bundle, start_d: Option<NaiveDate>) -> Vec<ComboRow> {
    if ledger.combos.is_empty() {
        return vec![];
    }
    let t = ledger.closed_df();
    let mut realized: HashMap<usize, f64> = HashMap::new();
    let mut last_close: HashMap<usize, Ts> = HashMap::new();
    let mut kinds: HashMap<usize, BTreeSet<&str>> = HashMap::new();
    for r in &t {
        if let Some(cid) = r.combo_id {
            *realized.entry(cid).or_insert(0.0) += r.pnl;
            let e = last_close.entry(cid).or_insert(r.close_time);
            if r.close_time > *e {
                *e = r.close_time;
            }
            kinds.entry(cid).or_default().insert(r.close_kind);
        }
    }
    let mark: HashMap<&str, f64> = open_pos.iter().map(|p| (p.symbol.as_str(), p.price)).collect();
    let mut unreal: HashMap<usize, f64> = HashMap::new();
    let mut open_qty: HashMap<usize, f64> = HashMap::new();
    for (sym, lots) in &ledger.open_lots {
        for l in lots {
            let Some(cid) = l.combo_id else { continue };
            let px = mark.get(sym.as_str()).copied().unwrap_or(NAN);
            *open_qty.entry(cid).or_insert(0.0) += l.qty.abs();
            if !px.is_nan() {
                *unreal.entry(cid).or_insert(0.0) += (px - l.price) * l.qty * l.multiplier;
            }
        }
    }
    let mut und_px: HashMap<String, Option<m::Closes>> = HashMap::new();
    for c in &ledger.combos {
        und_px.entry(c.underlying.clone()).or_insert_with(|| m::bar_closes(&b.bars, &c.underlying));
    }
    let mut rows = Vec::new();
    for c in &ledger.combos {
        let cid = c.id;
        let exps: Vec<NaiveDate> = c.legs.keys().filter_map(|s| parse_occ(s).map(|k| k.expiry)).collect();
        let open_d = ny_date(&c.open_time);
        let is_open = open_qty.get(&cid).copied().unwrap_or(0.0) > 1e-9;
        let empty = BTreeSet::new();
        let k = kinds.get(&cid).unwrap_or(&empty);
        let outcome = if is_open {
            "Open"
        } else if k.contains("assigned") {
            "Assigned"
        } else if k.contains("exercised") {
            "Exercised"
        } else if k.len() == 1 && k.contains("expired") {
            "Expired"
        } else if k.contains("expired") {
            "Closed / partly expired"
        } else {
            "Closed early"
        };
        let mr = max_risk(c);
        let pr = realized.get(&cid).copied().unwrap_or(0.0);
        let pu = unreal.get(&cid).copied().unwrap_or(0.0);
        let upx = und_px.get(&c.underlying).and_then(|s| s.as_ref()).and_then(|s| m::last_le(s, open_d)).unwrap_or(NAN);
        let mut moneyness = NAN;
        if c.legs.len() == 1 && !upx.is_nan() {
            let occ = parse_occ(c.legs.keys().next().unwrap()).unwrap();
            moneyness = if occ.is_call() { upx / occ.strike - 1.0 } else { occ.strike / upx - 1.0 };
        }
        let close_t = if is_open { None } else { last_close.get(&cid).copied() };
        let rights: BTreeSet<char> = c.legs.keys().filter_map(|s| parse_occ(s).map(|k| k.right)).collect();
        let hold = match close_t {
            Some(ct) => util::secs(&c.open_time, &ct) / 86400.0,
            None => NAN,
        };
        let pnl = pr + pu;
        rows.push(ComboRow {
            combo_id: cid,
            underlying: c.underlying.clone(),
            strategy: c.strategy.clone(),
            open_time: c.open_time,
            close_time: close_t,
            n_legs: c.legs.len(),
            legs: c.legs.iter().map(|(s, q)| format!("{}{} {}", if *q > 0.0 { "+" } else { "" }, util::g(*q), leg_label(s))).collect::<Vec<_>>().join(", "),
            net_premium: c.open_cash,
            premium_side: if c.open_cash > 0.0 { "Credit" } else { "Debit" },
            max_risk: mr,
            dte: exps.iter().min().map(|e| (*e - open_d).num_days()).unwrap_or(0),
            realized_pnl: pr,
            unrealized_pnl: pu,
            pnl,
            outcome,
            is_open,
            rolled_from: c.rolled_from,
            rolled_to: c.rolled_to,
            underlying_px: upx,
            moneyness,
            rights: rights.iter().collect(),
            hold_days: hold,
            ror: if mr > 0.0 { pnl / mr } else { NAN },
            campaign: cid,
        });
    }
    for r in rows.iter_mut() {
        let mut root = r.combo_id;
        let mut seen = HashSet::new();
        while let Some(f) = ledger.combo(root).rolled_from {
            if seen.contains(&root) {
                break;
            }
            seen.insert(root);
            root = f;
        }
        r.campaign = root;
    }
    if let Some(sd) = start_d {
        rows.retain(|r| r.is_open || r.close_time.map(|c| ny_date(&c) >= sd).unwrap_or(false));
    }
    rows.sort_by(|a, b| a.open_time.cmp(&b.open_time));
    rows
}

pub fn daily_fee_fallback(b: &Bundle) -> BTreeMap<NaiveDate, f64> {
    let mut out = BTreeMap::new();
    if b.activities.iter().any(|a| jv::s(a, "activity_type").to_uppercase() == "FEE") {
        return out;
    }
    let Some(ph) = &b.portfolio_history else { return out };
    let fees = jv::arr(ph.get("cashflow").and_then(|c| c.get("FEE")));
    if fees.is_empty() {
        return out;
    }
    let ts = jv::arr(ph.get("timestamp"));
    for (t, v) in ts.iter().zip(fees) {
        let v = jv::float_of(v).unwrap_or(0.0);
        if v != 0.0 && !v.is_nan() {
            out.insert(ny_date(&util::from_unix(jv::float_of(t).unwrap_or(0.0))), v);
        }
    }
    out
}

fn equity_start(b: &Bundle, ledger: &Ledger) -> Option<NaiveDate> {
    let ph = b.portfolio_history.as_ref().filter(|p| jv::truthy(Some(p)))?;
    m::equity_series(ph, ledger).ok().map(|e| e.dates[0])
}

fn cash_summary(ledger: &Ledger, start_d: Option<NaiveDate>) -> CashSummary {
    let mut cs = CashSummary::default();
    for r in ledger.cash_rows() {
        if start_d.map(|s| r.date < s).unwrap_or(false) {
            continue;
        }
        match r.kind {
            Kind::Fee => cs.fees += r.cash,
            Kind::Income => {
                cs.income += r.cash;
                *cs.income_by_symbol.entry(r.symbol.clone()).or_insert(0.0) += r.cash;
            }
            Kind::Interest => cs.interest += r.cash,
            Kind::Flow => cs.flows += r.cash,
            _ => {}
        }
    }
    cs
}

fn append_live_point(eq: EqFrame, b: &Bundle, ledger: &Ledger) -> EqFrame {
    let Some(acct) = &b.account else { return eq };
    let Some(equity) = jv::f(acct, "equity").filter(|_| jv::truthy(acct.get("equity"))) else { return eq };
    if ledger.events.is_empty() {
        return eq;
    }
    let Some(today) = ledger.events.iter().filter(|e| e.kind == Kind::Trade).map(|e| e.ny_date()).max() else { return eq };
    let last = *eq.dates.last().unwrap();
    if today <= last || today.weekday().num_days_from_monday() >= 5 || (today - last).num_days() > 5 {
        return eq;
    }
    let flows: f64 = ledger.events.iter().filter(|e| e.kind == Kind::Flow && e.ny_date() == today).map(|e| e.cash).sum();
    let mut eq = eq;
    eq.dates.push(today);
    eq.equity.push(equity);
    eq.flow.push(flows);
    eq
}

fn coverage_alerts(eq: &EqFrame, ledger: &Ledger) -> Vec<String> {
    let mut alerts = Vec::new();
    let trades: Vec<NaiveDate> = ledger.events.iter().filter(|e| e.kind == Kind::Trade).map(|e| e.ny_date()).collect();
    let last_ph = *eq.dates.last().unwrap();
    if let Some(mx) = trades.iter().max() {
        if (*mx - last_ph).num_days() > 7 {
            alerts.push(format!(
                "Portfolio history ends {}, but trades continue to {}. Account-level charts (equity, returns, drawdown, benchmarks) only cover the history that was fetched. Use <b>Re-download everything</b> to fetch it again.",
                last_ph.format("%Y-%m-%d"),
                mx.format("%Y-%m-%d")
            ));
        }
    }
    if !eq.funded {
        alerts.push("Every equity value in the portfolio history is $0 – the fetched window predates funding.".into());
    }
    if !eq.dropped.is_empty() {
        let tot: f64 = eq.dropped.iter().map(|x| x.1).sum();
        alerts.push(format!(
            "{} deposits/withdrawals (net ${}) fall after the end of the portfolio history and could not be counted.",
            eq.dropped.len(),
            util::commas(tot, 2)
        ));
    }
    alerts
}

fn symbol_scorecard(a: &Analysis) -> Vec<SymRow> {
    let t = &a.trades;
    if t.is_empty() {
        return vec![];
    }
    let mut df: BTreeMap<String, SymRow> = BTreeMap::new();
    for r in t {
        let e = df.entry(r.underlying.clone()).or_insert_with(|| SymRow { underlying: r.underlying.clone(), ..Default::default() });
        if r.is_option {
            e.option_pnl += r.pnl;
        } else {
            e.stock_pnl += r.pnl;
        }
        e.trades += 1.0;
        if r.pnl > 0.0 {
            e.wins += 1.0;
        }
    }
    let op = &a.open_positions;
    if !op.is_empty() {
        let mut u: BTreeMap<String, Option<f64>> = BTreeMap::new();
        for p in op {
            let e = u.entry(p.underlying.clone()).or_insert(None);
            if !p.unrealized_pl.is_nan() {
                *e = Some(e.unwrap_or(0.0) + p.unrealized_pl);
            }
        }
        for (k, v) in u {
            let e = df.entry(k.clone()).or_insert_with(|| SymRow { underlying: k.clone(), ..Default::default() });
            e.unrealized = v.unwrap_or(0.0);
        }
    }
    let open_unds: HashSet<&str> = op.iter().map(|p| p.underlying.as_str()).collect();
    let mut last_close: HashMap<String, NaiveDate> = HashMap::new();
    for r in t {
        let d = ny_date(&r.close_time);
        let e = last_close.entry(r.underlying.clone()).or_insert(d);
        if d > *e {
            *e = d;
        }
    }
    for e in df.values_mut() {
        e.dividends = a.cash_summary.income_by_symbol.get(&e.underlying).copied().unwrap_or(0.0);
        e.total_pnl = e.stock_pnl + e.option_pnl + e.unrealized + e.dividends;
        e.realized = e.stock_pnl + e.option_pnl;
        e.is_open = open_unds.contains(e.underlying.as_str());
    }
    // buy & hold comparison
    let mut stock_entries: HashMap<&str, (NaiveDate, f64)> = HashMap::new();
    let mut first_touch: HashMap<&str, NaiveDate> = HashMap::new();
    for e in &a.ledger.events {
        if e.kind == Kind::Trade {
            if !e.is_option && e.qty > 0.0 {
                stock_entries.entry(e.symbol.as_str()).or_insert((e.ny_date(), e.price));
            }
            first_touch.entry(e.underlying.as_str()).or_insert(e.ny_date());
        }
    }
    let peak = peak_capital(&a.ledger, None);
    let peak_c = peak_capital(&a.ledger, Some(&last_close));
    for (und, row) in df.iter_mut() {
        let Some(&d0) = first_touch.get(und.as_str()) else { continue };
        if a.start.map(|s| d0 < s).unwrap_or(false) {
            continue;
        }
        let closes = m::bar_closes(&a.bundle.bars, und);
        let mut p0 = closes.as_ref().and_then(|c| m::last_le(c, d0));
        if p0.is_none() {
            if let Some((d, px)) = stock_entries.get(und.as_str()) {
                if *d == d0 {
                    p0 = Some(*px);
                }
            }
        }
        let p0 = p0.filter(|v| *v != 0.0 && !v.is_nan());
        let p1 = a.marks.get(und).copied().filter(|v| *v != 0.0);
        let cap = peak.get(und).copied().unwrap_or(0.0);
        if let (Some(p0), Some(p1)) = (p0, p1) {
            if cap != 0.0 {
                row.bh_pnl = Some(cap * (p1 / p0 - 1.0));
            }
        }
        let cap_c = peak_c.get(und).copied().unwrap_or(0.0);
        if let (Some(&d1), Some(p0)) = (last_close.get(und), p0) {
            if cap_c != 0.0 {
                let mut p1c = closes.as_ref().and_then(|c| m::last_le(c, d1));
                if p1c.is_none() {
                    let st: Vec<&Trade> = t.iter().filter(|r| &r.underlying == und && !r.is_option).collect();
                    if let Some(last) = st.iter().max_by(|a, b| a.close_time.cmp(&b.close_time)) {
                        if ny_date(&last.close_time) == d1 {
                            p1c = Some(last.close_price);
                        }
                    }
                }
                if let Some(p1c) = p1c.filter(|v| *v != 0.0 && !v.is_nan()) {
                    row.bhc_end = Some(d1);
                    row.bhc_return = Some(p1c / p0 - 1.0);
                    row.bhc_capital = Some(cap_c);
                    row.bhc_pnl = Some(cap_c * (p1c / p0 - 1.0));
                    row.edge_closed = Some(row.realized - cap_c * (p1c / p0 - 1.0));
                }
            }
        }
    }
    let mut out: Vec<SymRow> = df.into_values().collect();
    out.sort_by(|a, b| a.total_pnl.partial_cmp(&b.total_pnl).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Peak gross capital simultaneously committed per underlying (stock cost + option premium).
pub fn peak_capital(ledger: &Ledger, until: Option<&HashMap<String, NaiveDate>>) -> HashMap<String, f64> {
    let mut peak: HashMap<String, f64> = HashMap::new();
    let mut pos: IndexMap<String, Vec<(f64, f64)>> = IndexMap::new();
    for e in &ledger.events {
        if !e.kind.is_position() {
            continue;
        }
        if let Some(u) = until {
            match u.get(&e.underlying) {
                Some(d) if e.ny_date() <= *d => {}
                _ => continue,
            }
        }
        let book = pos.entry(e.symbol.clone()).or_default();
        let mut q = e.qty;
        while q.abs() > 1e-9 && !book.is_empty() && (book[0].0 > 0.0) != (q > 0.0) {
            let take = q.abs().min(book[0].0.abs());
            if book[0].0 < 0.0 {
                book[0].0 += take;
            } else {
                book[0].0 -= take;
            }
            if q < 0.0 {
                q += take;
            } else {
                q -= take;
            }
            if book[0].0.abs() < 1e-9 {
                book.remove(0);
            }
        }
        if q.abs() > 1e-9 {
            book.push((q, e.price));
        }
        let mut tot = 0.0;
        for (sym, b) in &pos {
            if underlying_of(sym) == e.underlying {
                let mult = if parse_occ(sym).is_some() { 100.0 } else { 1.0 };
                tot += b.iter().map(|x| x.0.abs() * x.1 * mult).sum::<f64>();
            }
        }
        let p = peak.entry(e.underlying.clone()).or_insert(0.0);
        if tot > *p {
            *p = tot;
        }
    }
    peak
}

fn reconcile(ledger: &Ledger, b: &Bundle) -> Option<Recon> {
    if b.positions.is_empty() {
        return None;
    }
    let mine: HashMap<&str, f64> = ledger.open_lots.iter().map(|(s, l)| (s.as_str(), l.iter().map(|x| x.qty).sum())).collect();
    let mut theirs: HashMap<String, f64> = HashMap::new();
    for p in &b.positions {
        theirs.insert(jv::s(p, "symbol"), jv::f0(p, "qty"));
    }
    let mut syms: BTreeSet<String> = mine.keys().map(|s| s.to_string()).collect();
    syms.extend(theirs.keys().cloned());
    let mut rows = Vec::new();
    for s in syms {
        let a = mine.get(s.as_str()).copied().unwrap_or(0.0);
        let bq = theirs.get(&s).copied().unwrap_or(0.0);
        if (a - bq).abs() > 1e-6 {
            rows.push(RecRow { symbol: s, ledger_qty: a, broker_qty: bq, diff: a - bq });
        }
    }
    Some(Recon::Rows(rows))
}
