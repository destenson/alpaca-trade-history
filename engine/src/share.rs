//! A version of the analysis that is safe to publish: dollars normalized, deposits/positions/sizes/
//! intraday times removed. Percent metrics are unchanged.

use crate::analysis::{bench_stats, Analysis, CashSummary, Recon};
use crate::metrics::{self as m, EqFrame, StatInput};
use crate::util::{self, ny_midnight, round_dp};

/// '+3 10/16 1C, -1 11/20 3C' -> '+ 10/16 1C, − 11/20 3C'
pub fn strip_leg_qty(legs: &str) -> String {
    let c: Vec<char> = legs.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let digit = |k: usize| c.get(k).map(|x| x.is_ascii_digit()).unwrap_or(false);
    while i < c.len() {
        let prev_ok = i == 0 || !(c[i - 1].is_alphanumeric() || c[i - 1] == '_' || c[i - 1] == '.');
        if prev_ok {
            let mut j = i;
            let sign = if c[j] == '+' || c[j] == '-' {
                j += 1;
                Some(c[j - 1])
            } else {
                None
            };
            let ds = j;
            while digit(j) {
                j += 1;
            }
            if j > ds {
                let mut k = j;
                if c.get(k) == Some(&'.') && digit(k + 1) {
                    k += 1;
                    while digit(k) {
                        k += 1;
                    }
                }
                if c.get(k) == Some(&' ') && digit(k + 1) && digit(k + 2) && c.get(k + 3) == Some(&'/') && digit(k + 4) && digit(k + 5) && c.get(k + 6) == Some(&' ') {
                    out.push_str(if sign == Some('-') { "− " } else { "+ " });
                    i = k + 1;
                    continue;
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

pub fn make_shareable(a: &Analysis, notional: f64) -> Analysis {
    let mut s = a.clone();
    s.share = true;
    s.notional = notional;
    let avg_cap = match &a.equity {
        Some(e) if e.equity.iter().any(|x| *x > 0.0) => {
            let v: Vec<f64> = e.equity.iter().copied().filter(|x| *x > 0.0).collect();
            util::mean(&v)
        }
        _ if !a.trades.is_empty() => {
            let c: f64 = a.trades.iter().map(|t| t.cost).sum();
            if c != 0.0 {
                c
            } else {
                1.0
            }
        }
        _ => notional,
    };
    let k = notional / avg_cap;

    for t in s.trades.iter_mut() {
        t.pnl *= k;
        t.cost *= k;
        t.fees *= k;
        t.open_time = ny_midnight(&t.open_time);
        t.close_time = ny_midnight(&t.close_time);
        t.hold_days = round_dp(t.hold_days, 1);
        t.qty = f64::NAN;
    }
    s.combos.retain(|c| !c.is_open);
    for c in s.combos.iter_mut() {
        c.net_premium *= k;
        c.max_risk *= k;
        c.realized_pnl *= k;
        c.pnl *= k;
        c.unrealized_pnl = 0.0;
        c.legs = strip_leg_qty(&c.legs);
        c.open_time = ny_midnight(&c.open_time);
        c.close_time = c.close_time.map(|t| ny_midnight(&t));
    }
    for r in s.symbols.iter_mut() {
        r.stock_pnl *= k;
        r.option_pnl *= k;
        r.dividends *= k;
        r.realized *= k;
        r.unrealized = 0.0;
        r.total_pnl = r.stock_pnl + r.option_pnl + r.dividends;
        r.bh_pnl = None;
        r.bhc_end = None;
        r.bhc_return = None;
        r.bhc_capital = None;
        r.bhc_pnl = None;
        r.edge_closed = None;
    }
    s.symbols.retain(|r| r.trades > 0.0);
    s.symbols.sort_by(|a, b| a.total_pnl.partial_cmp(&b.total_pnl).unwrap_or(std::cmp::Ordering::Equal));

    let unreal: f64 = {
        let v: Vec<f64> = a.open_positions.iter().map(|p| p.unrealized_pl).filter(|x| !x.is_nan()).collect();
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum()
        }
    };
    s.open_positions.clear();
    let cs = &a.cash_summary;
    s.cash_summary = CashSummary {
        fees: cs.fees * k,
        fees_in_trades: cs.fees_in_trades * k,
        income: cs.income * k,
        interest: cs.interest * k,
        flows: notional,
        unrealized: unreal * k,
        income_by_symbol: cs.income_by_symbol.iter().map(|(kk, v)| (kk.clone(), v * k)).collect(),
    };

    if let Some(ae) = &a.equity {
        let mut e = EqFrame { dates: ae.dates.clone(), equity: ae.twr.iter().map(|t| notional * t).collect(), flow: vec![0.0; ae.len()], funded: true, ..Default::default() };
        e.flow[0] = e.equity[0];
        m::add_returns(&mut e);
        let mut bench = indexmap::IndexMap::new();
        for (sym, bf) in &a.bench {
            let mut nb = bf.clone();
            nb.value = nb.twr.iter().map(|t| notional * t).collect();
            nb.cum_pnl = nb.value.iter().map(|v| v - notional).collect();
            bench.insert(sym.clone(), nb);
        }
        let pb = bench.get(&a.primary_benchmark);
        s.stats = Some(m::perf_stats(&StatInput::of(&e), pb, 0.0));
        let mut bs = indexmap::IndexMap::new();
        for (sym, bf) in &bench {
            bs.insert(sym.clone(), bench_stats(bf, &e, 0.0, false));
        }
        s.bench_stats = bs;
        s.drawdowns = m::drawdown_periods(&e, 5);
        s.rolling = Some(m::rolling_stats(&e, pb, 21));
        s.monthly = m::monthly(&e.dates, &e.ret, Some(&e.pnl));
        s.bench = bench;
        s.equity = Some(e);
    }
    s.reconciliation = a.reconciliation.as_ref().map(|r| match r {
        Recon::Rows(rows) => Recon::Matched(rows.is_empty()),
        Recon::Matched(b) => Recon::Matched(*b),
    });
    s.warnings = vec![];
    s.alerts = a.alerts.iter().map(|x| strip_net(x)).collect();
    s
}

/// re.sub(r" \(net \$[^)]*\)", "", x)
fn strip_net(x: &str) -> String {
    let mut out = x.to_string();
    while let Some(i) = out.find(" (net $") {
        match out[i..].find(')') {
            Some(j) => out.replace_range(i..i + j + 1, ""),
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn strip() {
        assert_eq!(super::strip_leg_qty("+3 10/16 1C, -1 11/20 3C"), "+ 10/16 1C, − 11/20 3C");
    }
}
