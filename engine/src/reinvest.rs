//! 'Reinvest option profits in the underlying': the whole shares option profits would have bought.
//! Wins-only plan, plus a variant where losses come out of the same pot.

use chrono::NaiveDate;
use indexmap::IndexMap;
use serde_json::Value;

use crate::analysis::ComboRow;
use crate::metrics::{bar_closes, last_le};
use crate::util::ny_date;

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub shares: i64,
    pub cost: f64,
    pub cash: f64,
    pub buys: Vec<(NaiveDate, i64, f64)>,
}

impl Plan {
    fn add(&mut self, d: NaiveDate, amount: f64, price: Option<f64>) {
        self.cash += amount;
        if let Some(p) = price.filter(|p| *p != 0.0) {
            if self.cash >= p {
                let n = (self.cash / p + 1e-9).floor() as i64;
                self.shares += n;
                self.cost += n as f64 * p;
                self.cash -= n as f64 * p;
                self.buys.push((d, n, p));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub underlying: String,
    pub closed: usize,
    pub wins: usize,
    pub losses: usize,
    pub profits: f64,
    pub loss_total: f64,
    pub net: f64,
    pub shares: i64,
    pub cost: f64,
    pub avg_cost: f64,
    pub cash: f64,
    pub price_now: f64,
    pub value: f64,
    pub gain: f64,
    pub held: f64,
    pub net_shares: i64,
    pub net_cash: f64,
    pub unpriced: usize,
    pub short_by: f64,
}

pub fn reinvest_table(combos: &[ComboRow], bars: &Value, price_now: &dyn Fn(&str) -> Option<f64>, held: &IndexMap<String, f64>) -> Vec<Row> {
    let mut done: Vec<&ComboRow> = combos.iter().filter(|c| !c.is_open && c.close_time.is_some()).collect();
    done.sort_by(|a, b| a.close_time.cmp(&b.close_time));
    let mut groups: IndexMap<&str, Vec<&ComboRow>> = IndexMap::new();
    for c in done {
        groups.entry(c.underlying.as_str()).or_default().push(c);
    }
    let mut rows = Vec::new();
    for (und, g) in groups {
        let closes = bar_closes(bars, und);
        let (mut wins, mut net) = (Plan::default(), Plan::default());
        let mut unpriced = 0;
        for c in &g {
            let d = ny_date(&c.close_time.unwrap());
            let px = closes.as_ref().and_then(|cl| last_le(cl, d));
            if px.is_none() {
                unpriced += 1;
            }
            wins.add(d, c.realized_pnl.max(0.0), px);
            net.add(d, c.realized_pnl, px);
        }
        let now = price_now(und).filter(|v| *v != 0.0);
        let pnl: Vec<f64> = g.iter().map(|c| c.realized_pnl).collect();
        let profits: f64 = pnl.iter().filter(|p| **p > 0.0).sum();
        let held_q = held.get(und).copied().unwrap_or(0.0);
        rows.push(Row {
            underlying: und.to_string(),
            closed: g.len(),
            wins: pnl.iter().filter(|p| **p > 0.0).count(),
            losses: pnl.iter().filter(|p| **p < 0.0).count(),
            profits,
            loss_total: pnl.iter().filter(|p| **p < 0.0).sum(),
            net: pnl.iter().sum(),
            shares: wins.shares,
            cost: wins.cost,
            avg_cost: if wins.shares != 0 { wins.cost / wins.shares as f64 } else { f64::NAN },
            cash: wins.cash,
            price_now: now.unwrap_or(f64::NAN),
            value: now.map(|p| wins.shares as f64 * p).unwrap_or(f64::NAN),
            gain: match now {
                Some(p) if wins.shares != 0 => wins.shares as f64 * p - wins.cost,
                _ => f64::NAN,
            },
            held: held_q,
            net_shares: net.shares,
            net_cash: net.cash,
            unpriced,
            short_by: wins.shares as f64 - held_q,
        });
    }
    rows.sort_by(|a, b| b.profits.total_cmp(&a.profits).then(b.net.total_cmp(&a.net)));
    rows
}
