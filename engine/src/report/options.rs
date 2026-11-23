//! Options tab: strategy statistics, reinvest card, open positions read every way.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use indexmap::IndexMap;
use serde_json::{json, Value};

use super::trades::Group;
use super::{dollars_list, dur, iso, jf, jr, jvec, lim, pn, slug, spot_from, table, when, Cell, Report, Tbl, NEUTRAL, S};
use crate::analysis::{ComboRow, OpenPos};
use crate::interpret::{build_units, readings, OptRow};
use crate::orders as O;
use crate::reinvest::reinvest_table;
use crate::reprice as rp;
use crate::util::{self, esc, interp, money, num, pct};

impl<'a> Report<'a> {
    pub fn options(&mut self) {
        let a = self.a;
        let c = &a.combos;
        if c.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        let done: Vec<&ComboRow> = c.iter().filter(|r| !r.is_open).collect();
        let wins: Vec<f64> = done.iter().filter(|r| r.pnl > 0.0).map(|r| r.pnl).collect();
        let losses: Vec<f64> = done.iter().filter(|r| r.pnl <= 0.0).map(|r| r.pnl).collect();
        let sum = |f: &dyn Fn(&ComboRow) -> f64| c.iter().map(f).sum::<f64>();
        let rors: Vec<f64> = done.iter().map(|r| r.ror).collect();
        parts.push(Self::tiles(&[
            ("Options P&L".into(), money(sum(&|r| r.pnl), true, 0),
                format!("realized {} · open {}", money(sum(&|r| r.realized_pnl), true, 0), money(sum(&|r| r.unrealized_pnl), true, 0))),
            ("Strategies".into(), c.len().to_string(), format!("{} closed · {} open · {} rolls", done.len(), c.iter().filter(|r| r.is_open).count(), c.iter().filter(|r| r.rolled_from.is_some()).count())),
            ("Win rate".into(), pct(if done.is_empty() { f64::NAN } else { wins.len() as f64 / done.len() as f64 }, false, 0),
                format!("avg win {} · avg loss {}", money(util::mean(&wins), false, 0), money(util::mean(&losses), false, 0))),
            ("Median return on risk".into(), pct(util::median(&rors), true, 0), "defined-risk positions only".into()),
            ("Premium".into(), format!("{} paid", money(-c.iter().filter(|r| r.net_premium < 0.0).map(|r| r.net_premium).sum::<f64>(), false, 0)),
                format!("{} collected", money(c.iter().filter(|r| r.net_premium > 0.0).map(|r| r.net_premium).sum::<f64>(), false, 0))),
            ("Median DTE at entry".into(), format!("{:.0} d", util::median(&c.iter().map(|r| r.dte as f64).collect::<Vec<_>>())),
                format!("median hold {}", dur(util::median(&done.iter().map(|r| r.hold_days).collect::<Vec<_>>())))),
        ]));

        let open_note = "Open strategies count with their unrealized P&L.";
        let by_key = |key: &dyn Fn(&ComboRow) -> String| -> Vec<(String, f64)> {
            let mut m: BTreeMap<String, f64> = BTreeMap::new();
            for r in c {
                *m.entry(key(r)).or_insert(0.0) += r.pnl;
            }
            let mut v: Vec<(String, f64)> = m.into_iter().collect();
            v.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal));
            v
        };
        let grp = |key: &dyn Fn(&ComboRow) -> bool, lab: &str| Group { lab: lab.into(), tk: lab.into(), pnl: c.iter().filter(|r| key(r)).map(|r| r.pnl).collect() };
        let groups: Vec<Group> = by_key(&|r| r.strategy.clone()).into_iter().map(|(k, _)| grp(&|r| r.strategy == k, &k)).collect();
        parts.push(self.dist_fig("f_strat", groups, "P&L by strategy",
            &format!("Legs opened within the grouping window on one underlying form a strategy; covered calls are detected from shares held at entry. {}", open_note),
            true, "trades", "", "", None));

        // cumulative by strategy (top 5 + other)
        let mut absum: BTreeMap<String, f64> = BTreeMap::new();
        for r in c {
            *absum.entry(r.strategy.clone()).or_insert(0.0) += r.pnl.abs();
        }
        let mut top: Vec<(String, f64)> = absum.into_iter().collect();
        top.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<String> = top.into_iter().take(5).map(|x| x.0).collect();
        let now = self.now;
        let mut cc: Vec<(&ComboRow, String, util::Ts)> = c
            .iter()
            .map(|r| (r, if top.contains(&r.strategy) { r.strategy.clone() } else { "Other".to_string() }, r.close_time.unwrap_or(now)))
            .collect();
        cc.sort_by(|x, y| x.2.cmp(&y.2));
        let mut names = top.clone();
        if cc.iter().any(|x| x.1 == "Other") {
            names.push("Other".into());
        }
        let mut data = Vec::new();
        for (i, g) in names.iter().enumerate() {
            let rows: Vec<&(&ComboRow, String, util::Ts)> = cc.iter().filter(|x| &x.1 == g).collect();
            data.push(json!({"type": "scatter", "mode": "lines", "name": g, "x": rows.iter().map(|x| iso(&x.2)).collect::<Vec<_>>(),
                "y": jvec(&util::cumsum(&rows.iter().map(|x| x.0.pnl).collect::<Vec<_>>()), 2), "line": {"color": S[i], "width": 2, "shape": "hv"}}));
        }
        parts.push(self.fig("f_stratcum", data, json!({"yaxis": {"tickprefix": "$"}}), 320, "", "Cumulative P&L by strategy", "", true));

        let bucket = |vals: Vec<f64>, edges: &[f64], labels: &[&str]| -> Vec<Group> {
            labels
                .iter()
                .enumerate()
                .map(|(i, lab)| {
                    let (lo, hi) = (edges[i], edges[i + 1]);
                    let pnl = c
                        .iter()
                        .zip(&vals)
                        .filter(|(_, v)| !v.is_nan() && ((*v > &lo && *v <= &hi) || (i == 0 && **v == lo)))
                        .map(|(r, _)| r.pnl)
                        .collect();
                    Group { lab: lab.to_string(), tk: lab.to_string(), pnl }
                })
                .collect()
        };
        let g = bucket(c.iter().map(|r| r.dte as f64).collect(), &[-1.0, 0.0, 2.0, 7.0, 14.0, 30.0, 60.0, 10_000.0], &["0DTE", "1–2", "3–7", "8–14", "15–30", "31–60", "60+"]);
        parts.push(self.dist_fig("f_dte", g, "P&L by days-to-expiration at entry", open_note, false, "trades", "", "", Some(340)));
        let g = bucket(c.iter().map(|r| r.hold_days).collect(), &[-1.0, 1.0 / 24.0, 1.0, 3.0, 7.0, 21.0, 10_000.0], &["<1h", "1h–1d", "1–3d", "3–7d", "1–3w", "3w+"]);
        parts.push(self.dist_fig("f_holdopt", g, "P&L by time in trade", "Open strategies count the time held so far.", false, "trades", "", "", Some(340)));

        let kind = |r: &ComboRow| -> &'static str {
            match r.rights.as_str() {
                "C" => "Calls",
                "P" => "Puts",
                _ => "Calls + puts",
            }
        };
        let mut cp = Vec::new();
        for k in ["Calls", "Puts", "Calls + puts"] {
            if c.iter().any(|r| kind(r) == k) {
                cp.push(grp(&|r| kind(r) == k, k));
            }
        }
        for k in ["Debit", "Credit"] {
            if c.iter().any(|r| r.premium_side == k) {
                cp.push(grp(&|r| r.premium_side == k, &format!("{} trades", k)));
            }
        }
        parts.push(self.dist_fig("f_cp", cp, "Calls vs. puts · debit vs. credit", open_note, false, "trades", "", "", Some(340)));

        let mut oc: IndexMap<&str, usize> = IndexMap::new();
        for r in c {
            *oc.entry(r.outcome).or_insert(0) += 1;
        }
        let mut oc: Vec<(&str, usize)> = oc.into_iter().collect();
        oc.sort_by(|x, y| y.1.cmp(&x.1));
        let g: Vec<Group> = oc.iter().map(|(k, _)| grp(&|r| r.outcome == *k, k)).collect();
        parts.push(self.dist_fig("f_outcome", g, "How positions ended", "", false, "trades", "", "", Some(340)));

        let g: Vec<Group> = by_key(&|r| r.underlying.clone()).into_iter().map(|(k, _)| grp(&|r| r.underlying == k, &k)).collect();
        parts.push(self.dist_fig("f_optund", g, "Options P&L by underlying", open_note, true, "trades", "", "", None));
        if !self.share {
            parts.push(self.reinvest_card());
        }

        let rr: Vec<f64> = done.iter().map(|r| r.ror).filter(|v| !v.is_nan()).collect();
        if rr.len() > 2 {
            let data = vec![json!({"type": "histogram", "x": jvec(&rr.iter().map(|v| v.clamp(-1.0, 3.0)).collect::<Vec<_>>(), 4), "xbins": {"size": 0.25},
                "marker": {"color": S[0], "line": {"color": "var:surface", "width": 2}}, "showlegend": false, "hovertemplate": "%{x}: %{y} trades<extra></extra>"})];
            parts.push(self.fig("f_ror", data, json!({"xaxis": {"tickformat": "+.0%", "title": {"text": "P&L ÷ max risk"}}, "yaxis": {"title": {"text": "Trades"}}}), 280, "",
                "Return on risk distribution", "−100% = full loss of the premium or spread width at risk.", false));
        }

        let mrows: Vec<&ComboRow> = c.iter().filter(|r| !r.moneyness.is_nan()).collect();
        if mrows.len() > 3 {
            let (myf, mh) = if self.share { (json!({"tickformat": "+.0%"}), "%{y:+.0%}") } else { (json!({"tickprefix": "$"}), "%{y:$,.2f}") };
            let ys: Vec<f64> = mrows.iter().map(|r| if self.share { r.ror } else { r.pnl }).collect();
            let data = vec![json!({"type": "scatter", "mode": "markers", "x": jvec(&mrows.iter().map(|r| r.moneyness).collect::<Vec<_>>(), 4), "y": jvec(&ys, 4),
                "text": mrows.iter().map(|r| r.legs.clone()).collect::<Vec<_>>(), "marker": {"color": mrows.iter().map(|r| pn(r.pnl)).collect::<Vec<_>>(), "size": 9},
                "showlegend": false, "hovertemplate": format!("%{{text}}<br>moneyness %{{x:+.1%}}<br>{}<extra></extra>", mh)})];
            parts.push(self.fig("f_money", data, json!({"xaxis": {"tickformat": "+.0%", "title": {"text": "Moneyness at entry (+ = ITM)"}}, "yaxis": myf}), 300, "",
                "Single-leg P&L vs. moneyness at entry", "", false));
        }

        // rolls
        let rolled: Vec<usize> = c.iter().filter(|r| r.rolled_from.is_some()).map(|r| r.campaign).collect();
        let camp: Vec<&ComboRow> = c.iter().filter(|r| rolled.contains(&r.campaign)).collect();
        if !camp.is_empty() {
            let mut by: BTreeMap<usize, Vec<&ComboRow>> = BTreeMap::new();
            for r in &camp {
                by.entry(r.campaign).or_default().push(r);
            }
            let mut tb = Tbl::new(&["Underlying", "Positions", "Start", "Total P&L", "Path"]);
            if !self.share {
                tb = tb.f("Total P&L", super::fm(true, 2));
            }
            for rows in by.values() {
                let p: f64 = rows.iter().map(|r| r.pnl).sum();
                let start = rows.iter().map(|r| r.open_time).min().unwrap();
                tb.push(vec![
                    Cell::s(&rows[0].underlying),
                    Cell::I(rows.len() as i64),
                    Cell::s(iso(&start)),
                    if self.share { Cell::s(if p > 0.0 { "gain" } else if p < 0.0 { "loss" } else { "flat" }) } else { Cell::F(p) },
                    Cell::s(rows.iter().map(|r| r.legs.as_str()).collect::<Vec<_>>().join("  →  ")),
                ]);
            }
            parts.push(format!(
                "<figure class=\"card wide\"><h3>Roll chains</h3><p class=\"sub\">A position opened within the window of closing another on the same underlying is treated as a roll.</p>{}</figure>",
                table(&tb, "t_rolls", &["Total P&L"])
            ));
        }

        parts.push(self.open_options());

        let mut tb = Tbl::new(&["Opened", "Underlying", "Strategy", "Legs", "DTE", "Net premium", "Max risk", "Outcome", "Days", "P&L", "P&L if filled", "Return on risk"])
            .f("Net premium", super::fm(true, 2))
            .f("Max risk", super::fm(false, 2))
            .f("Days", super::fnum(1))
            .f("P&L", super::fm(true, 2))
            .f("P&L if filled", super::fm(true, 2))
            .f("Return on risk", super::fp(true, 0));
        let mut rows: Vec<&ComboRow> = c.iter().collect();
        rows.sort_by(|x, y| y.open_time.cmp(&x.open_time));
        let closers = if self.share { HashMap::new() } else { self.close_targets() };
        for r in rows {
            tb.push(vec![
                Cell::s(iso(&r.open_time)),
                Cell::s(&r.underlying),
                Cell::s(&r.strategy),
                Cell::s(&r.legs),
                Cell::I(r.dte),
                Cell::F(r.net_premium),
                Cell::F(r.max_risk),
                Cell::s(r.outcome),
                Cell::F(r.hold_days),
                match closers.get(&r.combo_id) {
                    Some((id, refs, _)) => Cell::OrderF(r.pnl, id.clone(), refs.clone()),
                    None => Cell::F(r.pnl),
                },
                // realized so far + closing the rest at the open closing order's limit
                match closers.get(&r.combo_id) {
                    Some((id, refs, Some(f))) => {
                        let mut v: Value = serde_json::from_str(refs).unwrap_or(json!({}));
                        v["target"] = Value::Null;
                        v["profit"] = json!(true);
                        Cell::OrderF(r.realized_pnl + f, id.clone(), v.to_string())
                    }
                    _ => Cell::N,
                },
                Cell::F(r.ror),
            ]);
        }
        let mut sub = "Net premium: blue = credit received, red = debit paid, fees included. Open positions include unrealized P&L; click an open position's P&L to close it (or change its closing order) at a chosen profit. P&L if filled: realized so far + closing the rest at the open closing order's limit.";
        if self.share {
            tb.drop_cols(&["Net premium", "Max risk", "P&L", "P&L if filled"]);
            sub = "Return on risk = P&L ÷ max risk at entry. Per-position dollar amounts are omitted in the shared view.";
        }
        parts.push(format!("<figure class=\"card wide\"><h3>All option strategies</h3><p class=\"sub\">{}</p>{}</figure>", sub, table(&tb, "t_strats", &["P&L", "P&L if filled", "Return on risk"])));
        self.section("options", "Options", parts.concat());
    }

    fn reinvest_card(&mut self) -> String {
        let a = self.a;
        let mut held: IndexMap<String, f64> = IndexMap::new();
        for p in a.open_positions.iter().filter(|p| !p.is_option) {
            *held.entry(p.symbol.clone()).or_insert(0.0) += p.qty;
        }
        let df = reinvest_table(&a.combos, &a.bundle.bars, &|u| spot_from(a, u), &held);
        let df: Vec<_> = df.into_iter().filter(|r| r.profits > 0.0 || r.held != 0.0).collect();
        if df.is_empty() {
            return String::new();
        }
        let diff = |c: &Cell| {
            let v = c.f();
            if v.is_nan() || v.abs() < 1e-9 {
                "on plan".to_string()
            } else if v > 0.0 {
                format!("buy {}", util::g(v))
            } else {
                format!("{} extra", util::g(-v))
            }
        };
        let mut tb = Tbl::new(&["Underlying", "Closed (W/L)", "Profits from wins", "Shares", "Shares (L)", "Avg cost", "Cash left", "Cash left (L)", "Price now", "Value now", "Gain on shares", "You hold", "Vs. plan"])
            .f("Profits from wins", super::fm(false, 2))
            .f("Shares", super::fg())
            .f("Avg cost", super::fm(false, 2))
            .f("Cash left", super::fm(false, 2))
            .f("Price now", super::fm(false, 2))
            .f("Value now", super::fm(false, 2))
            .f("Gain on shares", super::fm(true, 2))
            .f("You hold", super::fg())
            .f("Vs. plan", diff)
            .f("Shares (L)", super::fg())
            .f("Cash left (L)", super::fm(true, 2));
        for r in &df {
            tb.push(vec![
                Cell::s(&r.underlying),
                Cell::s(format!("{} / {}", r.wins, r.losses)),
                Cell::F(r.profits),
                Cell::I(r.shares),
                Cell::I(r.net_shares),
                Cell::F(r.avg_cost),
                Cell::F(r.cash),
                Cell::F(r.net_cash),
                Cell::F(r.price_now),
                Cell::F(if r.shares > 0 { r.value } else { f64::NAN }),
                Cell::F(r.gain),
                Cell::F(r.held),
                Cell::F(r.short_by),
            ]);
        }
        let to_buy: Vec<_> = df.iter().filter(|r| r.short_by > 0.0).collect();
        let buy_cost: f64 = to_buy.iter().map(|r| r.short_by * r.price_now).filter(|v| !v.is_nan()).sum();
        let tot_cost: f64 = df.iter().map(|r| r.cost).sum();
        let tot_val: f64 = df.iter().map(|r| r.value).filter(|v| !v.is_nan()).sum();
        let unpriced: usize = df.iter().map(|r| r.unpriced).sum();
        let sub = format!(
            "Profits from {} winning positions ({}) would have bought shares costing {}, worth {} now ({}). Matching the plan means buying {} shares across {} names for about {} at current prices.",
            df.iter().map(|r| r.wins).sum::<usize>(),
            money(df.iter().map(|r| r.profits).sum(), false, 0),
            money(tot_cost, false, 0),
            money(tot_val, false, 0),
            money(tot_val - tot_cost, true, 0),
            to_buy.iter().map(|r| r.short_by).sum::<f64>().trunc() as i64,
            to_buy.len(),
            money(buy_cost, false, 0)
        );
        let mut note = "How it's counted: each closed winning option position adds its profit to a pot for that underlying on the day it closed; whenever the pot covers a share, whole shares are bought at that day's close and the rest stays in the pot (<b>Cash left</b>). Losing positions are ignored. <b>(L)</b> columns count losses too: they come out of the same pot, so they have to be earned back before more shares are bought (shares already bought are kept; red cash = losses still to recover). <b>Vs. plan</b> compares with all the shares you hold now, however you bought them.".to_string();
        if unpriced > 0 {
            note.push_str(&format!(" {} closing day(s) had no price history, so no shares were bought then.", unpriced));
        }
        format!(
            "<figure class=\"card wide\"><h3>Option profits reinvested in the underlying</h3><p class=\"sub\">{}</p>{}<p class=\"note\">{}</p></figure>",
            sub,
            table(&tb, "t_reinvest", &["Gain on shares", "Cash left (L)"]),
            note
        )
    }

    fn open_options(&mut self) -> String {
        let a = self.a;
        let op = &a.open_positions;
        if op.is_empty() || !op.iter().any(|p| p.is_option) {
            return String::new();
        }
        let today = a.equity.as_ref().map(|e| *e.dates.last().unwrap()).unwrap_or_else(|| self.today());
        let oo: Vec<&OpenPos> = op.iter().filter(|p| p.is_option && p.expiry.map(|e| e >= today).unwrap_or(false)).collect();
        let mut by: BTreeMap<&str, Vec<&OpenPos>> = BTreeMap::new();
        for p in &oo {
            by.entry(p.underlying.as_str()).or_default().push(p);
        }
        let mut out = Vec::new();
        struct GRow {
            und: String,
            contract: String,
            qty: f64,
            iv: Option<f64>,
            d: f64,
            g: f64,
            t: f64,
            v: f64,
        }
        let mut greek_rows: Vec<GRow> = Vec::new();
        for (i, (und, g)) in by.iter().enumerate() {
            let mspot = self.market().and_then(|m| m.underlyings.get(*und)).and_then(|u| u.spot).filter(|s| *s != 0.0);
            let spot = mspot.or_else(|| spot_from(a, und));
            for r in g {
                let snap = a.bundle.option_snapshots.get(&r.symbol).unwrap_or(crate::jv::empty_obj());
                let iv = crate::jv::f(snap, "impliedVolatility");
                if let Some(gk) = crate::jv::obj(snap.get("greeks")).filter(|o| !o.is_empty()) {
                    let gv = |k: &str| gk.get(k).and_then(crate::jv::float_of).unwrap_or(0.0) * 100.0 * r.qty;
                    greek_rows.push(GRow { und: und.to_string(), contract: super::leg(&r.symbol), qty: r.qty, iv, d: gv("delta"), g: gv("gamma"), t: gv("theta"), v: gv("vega") });
                }
            }
            let sh: Vec<&OpenPos> = op.iter().filter(|p| !p.is_option && p.symbol == *und).collect();
            let shares: f64 = sh.iter().map(|p| p.qty).sum();
            let share_cost: f64 = sh.iter().map(|p| p.cost_basis).sum();
            let share_value: f64 = sh.iter().map(|p| if p.market_value.is_nan() { 0.0 } else { p.market_value }).sum();
            if shares != 0.0 {
                greek_rows.push(GRow { und: und.to_string(), contract: "shares".into(), qty: shares, iv: None, d: shares, g: 0.0, t: 0.0, v: 0.0 });
            }
            let Some(spot) = spot else { continue };
            out.push(self.readings_card(i, und, g, spot, today, shares, share_cost, share_value));
        }
        let mut html_g = String::new();
        if !greek_rows.is_empty() {
            let mut tot: BTreeMap<String, [f64; 4]> = BTreeMap::new();
            for r in &greek_rows {
                let e = tot.entry(r.und.clone()).or_insert([0.0; 4]);
                e[0] += r.d;
                e[1] += r.g;
                e[2] += r.t;
                e[3] += r.v;
            }
            for (und, v) in tot {
                greek_rows.push(GRow { und, contract: "TOTAL".into(), qty: f64::NAN, iv: None, d: v[0], g: v[1], t: v[2], v: v[3] });
            }
            greek_rows.sort_by(|x, y| (x.und.clone(), x.contract != "TOTAL", x.contract.clone()).cmp(&(y.und.clone(), y.contract != "TOTAL", y.contract.clone())));
            let mut tb = Tbl::new(&["Underlying", "Contract", "Qty", "IV", "Δ shares", "Γ", "Θ $/day", "Vega $/vol-pt"])
                .f("Qty", |c| if c.f().is_nan() { String::new() } else { util::g(c.f()) })
                .f("IV", |c| if c.f().is_nan() { String::new() } else { pct(c.f(), false, 0) })
                .f("Δ shares", super::fnum(1))
                .f("Γ", super::fnum(2))
                .f("Θ $/day", super::fm(true, 2))
                .f("Vega $/vol-pt", super::fm(true, 2));
            // one collapsible group per underlying: the TOTAL row shows, its contracts and shares expand
            for r in &greek_rows {
                let total = r.contract == "TOTAL";
                let contract = if total {
                    let n = greek_rows.iter().filter(|x| x.und == r.und && x.contract != "TOTAL" && x.contract != "shares").count();
                    let sh = greek_rows.iter().any(|x| x.und == r.und && x.contract == "shares");
                    format!("Total · {} contract{}{}", n, if n == 1 { "" } else { "s" }, if sh { " + shares" } else { "" })
                } else {
                    r.contract.clone()
                };
                tb.push_grouped(vec![Cell::s(&r.und), Cell::s(contract), Cell::F(r.qty), Cell::of(r.iv), Cell::F(r.d), Cell::F(r.g), Cell::F(r.t), Cell::F(r.v)], &r.und, total);
            }
            html_g = format!(
                "<figure class=\"card wide\"><h3>Open option Greeks</h3><p class=\"sub\">Position Greeks from Alpaca snapshots, totalled per underlying: ▸ shows the contracts and shares behind a total. Δ in share-equivalents; Θ is dollars per day; vega is dollars per 1 vol point.</p>{}</figure>",
                table(&tb, "t_greeks", &["Θ $/day"])
            );
        }
        let card = if out.is_empty() {
            String::new()
        } else {
            let unds: Vec<String> = by.keys().map(|s| s.to_string()).collect();
            self.rp_card(&unds, "Edit a price or apply an IV change and the estimated value and the dotted curve of every reading for that underlying update, here and on the Orders tab.")
        };
        format!("{}{}{}", card, out.concat(), html_g)
    }

    #[allow(clippy::too_many_arguments)]
    fn readings_card(&mut self, i: usize, und: &str, g: &[&OpenPos], spot: f64, today: NaiveDate, shares: f64, share_cost: f64, share_value: f64) -> String {
        let a = self.a;
        let rows: Vec<OptRow> = g
            .iter()
            .map(|r| OptRow { symbol: r.symbol.clone(), qty: r.qty, cost_basis: r.cost_basis, market_value: if r.market_value.is_nan() { 0.0 } else { r.market_value } })
            .collect();
        let (units, odd, split) = build_units(&rows, shares, share_cost, share_value, &a.bundle.option_snapshots);
        let mut by_combo: IndexMap<String, Vec<String>> = IndexMap::new();
        for r in g {
            for lot in a.ledger.open_lots.get(&r.symbol).map(|v| v.as_slice()).unwrap_or(&[]) {
                let n = if split { lot.qty.abs().round() as usize } else { 1 };
                let key = lot.combo_id.map(|c| c.to_string()).unwrap_or_else(|| format!("solo-{}", r.symbol));
                by_combo.entry(key).or_default().extend(std::iter::repeat(r.symbol.clone()).take(n.max(1)));
            }
        }
        let opened: Vec<Vec<String>> = by_combo.into_values().collect();
        let (rs, mut whole) = readings(&units, spot, today, Some(&opened));
        if rs.is_empty() {
            return String::new();
        }
        let n_legs = g.len();
        let snaps = &a.bundle.option_snapshots;
        let mut model = None;
        let mut est_unreal = 0.0;
        let mut is_open = false;
        let mut when_s = String::new();
        if let Some(m) = self.market().filter(|m| m.underlyings.contains_key(und)).cloned() {
            let ur: Vec<&crate::interpret::Unit> = units.iter().collect();
            let models = O::models_for(&ur, snaps, Some(&m), und, &rp::underlying_ivs(snaps));
            whole.today = O::curve_open(&ur, &models, &whole.x, 0.0);
            model = Some(O::model_payload(&ur, &models));
            est_unreal = interp(spot, &whole.x, &whole.today);
            is_open = m.is_open;
            when_s = when(&m);
        }
        let lot_shares = shares - odd.as_ref().map(|o| o.qty).unwrap_or(0.0);
        let stock_note = if lot_shares != 0.0 { format!(" + {} shares", util::g(lot_shares)) } else { String::new() };
        let odd_note = match &odd {
            Some(o) => {
                let q = o.qty.abs();
                format!(
                    " {} odd-lot share{} (not a multiple of 100) {} left out: shares only count toward a strategy in 100-share lots, one per contract.",
                    util::g(q),
                    if q != 1.0 { "s" } else { "" },
                    if q != 1.0 { "are" } else { "is" }
                )
            }
            None => String::new(),
        };
        let mut head = format!(
            "Whole position: max gain {} · max loss {} · breakevens {} · unrealized {}{}",
            lim(whole.max_gain),
            super::lim_loss(whole.max_loss),
            if whole.breakevens.is_empty() { "none".to_string() } else { dollars_list(&whole.breakevens) },
            money(whole.unrealized, true, 2),
            match whole.greeks.theta.filter(|t| *t != 0.0) {
                Some(t) => format!(" · Δ {} sh · Θ {}/day", num(whole.greeks.delta.unwrap_or(0.0), 1), money(t, true, 2)),
                None => String::new(),
            }
        );
        if let Some(mp) = &model {
            head.push_str(&format!(
                "<br><span class=\"rp-est\" id=\"est_rd_{}\">Estimated at {}: unrealized <b>{}</b> · value {}</span>",
                i,
                when_s,
                money(est_unreal, true, 2),
                money(est_unreal + mp.cost, true, 2)
            ));
        }
        let orders = if self.share { Vec::new() } else { O::parse_orders(&a.bundle.orders) };
        // the option legs of a group, summed per contract (None when it holds shares: no multi-leg order)
        let legs_of = |units: &[crate::interpret::Unit]| -> Option<Vec<(String, f64)>> {
            let mut m: BTreeMap<String, f64> = BTreeMap::new();
            for u in units {
                if u.symbol == crate::interpret::STOCK {
                    return None;
                }
                *m.entry(u.symbol.clone()).or_insert(0.0) += u.qty;
            }
            let v: Vec<(String, f64)> = m.into_iter().filter(|x| x.1.abs() > 1e-9).collect();
            (!v.is_empty()).then_some(v)
        };
        let mut opts = Vec::new();
        let mut views = Vec::new();
        for (j, r) in rs.iter().enumerate() {
            let vid = format!("iv_{}_{}", i, j);
            let mut lbls: Vec<String> = r.groups.iter().map(|g| g.legs_label()).collect();
            lbls.sort();
            let label = r.label();
            let mut parts: Vec<&str> = vec![und, &label];
            parts.extend(lbls.iter().map(|s| s.as_str()));
            let fid = format!("f_read_{}", slug(&parts));
            let tag = if r.as_opened { "As opened: " } else { "" };
            let warn = if r.undefined() > 0 { format!(" ({} undefined-risk)", r.undefined()) } else { String::new() };
            opts.push(format!("<option value=\"{}\">{}</option>", vid, esc(&format!("{}{}{}", tag, label, warn))));
            let mut data = Vec::new();
            for (k, grp) in r.groups.iter().enumerate() {
                data.push(json!({"type": "scatter", "mode": "lines", "name": format!("{} ({})", grp.strategy, grp.legs_label()), "x": super::jraw(&grp.curve_x), "y": super::jraw(&grp.curve_expiry), "line": {"color": S[k % 8], "width": 2}}));
            }
            data.push(json!({"type": "scatter", "mode": "lines", "name": "Whole position at expiration", "x": super::jraw(&whole.x), "y": super::jraw(&whole.expiry), "line": {"color": "var:ink", "width": 2, "dash": "dash"}}));
            let nm = if model.is_some() {
                if is_open { "Whole position now (est.)" } else { "Whole position at next open (est.)" }
            } else {
                "Whole position today"
            };
            data.push(json!({"type": "scatter", "mode": "lines", "name": nm, "x": super::jraw(&whole.x), "y": super::jraw(&whole.today), "line": {"color": NEUTRAL, "width": 2, "dash": "dot"}}));
            if let Some(mp) = &model {
                data.push(json!({"type": "scatter", "mode": "markers", "name": "Estimated P&L at that price", "x": [jf(spot)], "y": [jr(est_unreal, 2)],
                    "marker": {"color": "var:ink", "size": 9, "symbol": "circle-open", "line": {"width": 2}}, "hovertemplate": "est. P&L %{y:$,.2f}<extra></extra>"}));
                self.rp_views.push(json!({"kind": "pos", "fid": fid, "und": und, "x": super::jraw(&whole.x), "order": mp.to_json(), "pos": Value::Null, "net": 0.0, "um": Value::Null,
                    "t_open": data.len() - 2, "t_mark": data.len() - 1, "t_comb": Value::Null, "est": format!("est_rd_{}", i), "rows": []}));
            }
            // open closing orders: the P&L they lock in, and where today's value curve reaches it
            if !self.share {
                let mut seen: Vec<String> = Vec::new();
                let mut cands: Vec<(Vec<(String, f64)>, f64, &[f64], &[f64], String)> = Vec::new();
                if let Some(l) = legs_of(&units) {
                    cands.push((l, whole_cost(&units), &whole.x, &whole.today, "whole position".into()));
                }
                for grp in &r.groups {
                    if let Some(l) = legs_of(&grp.units) {
                        cands.push((l, grp.cost, &grp.curve_x, &grp.curve_today, grp.strategy.clone()));
                    }
                }
                for (l, basis, xs, now, what) in cands {
                    let (id, _, fill) = closer(&l, basis, f64::NAN, &orders, "0");
                    let Some((pnl, net)) = fill else { continue };
                    if seen.contains(&id) {
                        continue;
                    }
                    seen.push(id);
                    let x0 = *whole.x.first().unwrap_or(&0.0);
                    let x1 = *whole.x.last().unwrap_or(&0.0);
                    let price = format!("${:.2} {}", net.abs(), if net < 0.0 { "credit" } else { "debit" });
                    data.push(json!({"type": "scatter", "mode": "lines", "name": format!("Closing order ({}) at {}: {} if filled", what, price, money(pnl, true, 2)),
                        "x": [jf(x0), jf(x1)], "y": [jr(pnl, 2), jr(pnl, 2)], "line": {"color": "var:series-6", "width": 2, "dash": "dashdot"},
                        "hovertemplate": format!("closing order at {}: %{{y:$,.2f}}<extra></extra>", price)}));
                    // crossings of today's value curve with that P&L: roughly where the order would fill now
                    let mut cx = Vec::new();
                    if xs.len() == now.len() {
                        for k in 1..xs.len() {
                            let (a0, a1) = (now[k - 1] - pnl, now[k] - pnl);
                            if a0.is_finite() && a1.is_finite() && (a0 == 0.0 || (a0 < 0.0) != (a1 < 0.0)) {
                                cx.push(xs[k - 1] + (xs[k] - xs[k - 1]) * if a1 != a0 { -a0 / (a1 - a0) } else { 0.0 });
                            }
                        }
                    }
                    if !cx.is_empty() {
                        data.push(json!({"type": "scatter", "mode": "markers", "name": "Fills about here today (est.)", "x": cx.iter().map(|v| jr(*v, 2)).collect::<Vec<_>>(),
                            "y": cx.iter().map(|_| jr(pnl, 2)).collect::<Vec<_>>(), "marker": {"color": "var:series-6", "size": 10, "symbol": "diamond"},
                            "hovertemplate": format!("{} at %{{x:$,.2f}}: closing order at {} fills (est.)<extra></extra>", und, price)}));
                    }
                }
            }
            let shapes = json!([{"type": "line", "x0": spot, "x1": spot, "yref": "paper", "y0": 0, "y1": 1, "line": {"color": "var:muted", "width": 1, "dash": "dot"}},
                                {"type": "line", "xref": "paper", "x0": 0, "x1": 1, "y0": 0, "y1": 0, "line": {"color": "var:axis", "width": 1}}]);
            let fig = self.fig(&fid, data, json!({"shapes": shapes, "hovermode": "x unified", "showlegend": true,
                "legend": {"orientation": "h", "y": -0.22, "yanchor": "top"},
                "xaxis": {"tickprefix": "$", "title": {"text": format!("{} price at expiration", und)}}, "yaxis": {"tickprefix": "$"},
                "annotations": [{"x": spot, "yref": "paper", "y": 1, "text": format!("spot ${}", util::commas(spot, 2)), "showarrow": false, "yanchor": "bottom", "font": {"size": 11}}]}),
                380, "", "", "", false);
            let mut tb = Tbl::new(&["Strategy", "Legs", "Cost", "Value", "Unrealized", "Max gain", "Max loss", "Breakevens", "Δ sh", "Θ $/day"])
                .f("Cost", super::fm(true, 2))
                .f("Value", super::fm(true, 2))
                .f("Unrealized", super::fm(true, 2))
                .f("Max gain", super::flim())
                .f("Max loss", super::flim_loss())
                .f("Δ sh", super::fnum(1))
                .f("Θ $/day", super::fm(true, 2));
            for grp in &r.groups {
                tb.push(vec![
                    Cell::s(&grp.strategy),
                    Cell::s(grp.legs_label()),
                    Cell::F(grp.cost),
                    Cell::F(grp.value),
                    match legs_of(&grp.units).filter(|_| !self.share) {
                        Some(l) => {
                            let (id, refs, _) = closer(&l, grp.cost, grp.value, &orders, &format!("{}{}", i, j));
                            Cell::OrderF(grp.unrealized(), id, refs)
                        }
                        None => Cell::F(grp.unrealized()),
                    },
                    Cell::F(grp.max_gain),
                    Cell::F(grp.max_loss),
                    Cell::s(if grp.breakevens.is_empty() { "–".to_string() } else { dollars_list(&grp.breakevens) }),
                    Cell::F(grp.greeks.delta.unwrap_or(f64::NAN)),
                    Cell::F(grp.greeks.theta.unwrap_or(f64::NAN)),
                ]);
            }
            let sum_loss = r.sum_max_loss();
            let n_strat = r.groups.iter().filter(|g| g.strategy != "Odd-lot shares").count();
            let summary = format!(
                "<p class=\"sub\">{} strateg{} · sum of each one's own max loss {}{}.</p>",
                n_strat,
                if n_strat != 1 { "ies" } else { "y" },
                lim(sum_loss),
                if n_strat > 1 && !sum_loss.is_infinite() {
                    " (margin-style worst case: the groups' worst prices differ, so this overstates what the legs can lose together, but it's what you could be left exposed to after closing some groups and keeping others)"
                } else {
                    ""
                }
            );
            views.push(format!(
                "<div class=\"interp-view\" id=\"{}\"{}>{}{}{}</div>",
                vid,
                if j == 0 { "" } else { " hidden" },
                summary,
                table(&tb, &format!("t_read_{}_{}", i, j), &["Unrealized"]),
                fig
            ));
        }
        let more = if split { "" } else { " Legs with several contracts are kept whole here (too many contracts to split every way)." };
        format!(
            "<figure class=\"card wide\"><h3>{}: {} way{} to read {} open leg{}{}</h3><p class=\"sub\">{}</p><label class=\"pick\">Interpretation <select class=\"interp\" data-box=\"ib_{}\">{}</select></label><div id=\"ib_{}\">{}</div><p class=\"note\">Every reading is the same legs, so total P&L is identical. What changes is how risk is grouped: per-group max loss and breakevens, and what you'd be left holding if you closed one group. Expiration curves value all legs at intrinsic, ignoring time value left on later-dated legs of calendars/diagonals.{}{}</p></figure>",
            super::ticker(und),
            rs.len(),
            if rs.len() != 1 { "s" } else { "" },
            n_legs,
            if n_legs != 1 { "s" } else { "" },
            stock_note,
            head,
            i,
            opts.concat(),
            i,
            views.concat(),
            more,
            odd_note
        )
    }
}

impl<'a> Report<'a> {
    /// For each open option position (combo): what the change-price popup needs to close it at a chosen
    /// profit. If an open order already closes exactly its legs, that order (to change); otherwise a new
    /// closing order ("new:<combo>") with the legs reversed. Profit is measured against the open lots'
    /// cost, like the unrealized P&L.
    fn close_targets(&self) -> HashMap<usize, (String, String, Option<f64>)> {
        let a = self.a;
        let mark: HashMap<&str, f64> = a.open_positions.iter().map(|p| (p.symbol.as_str(), p.price)).collect();
        let mut legs: BTreeMap<usize, BTreeMap<String, (f64, f64, f64)>> = BTreeMap::new();   // qty, cost, value
        for (sym, lots) in &a.ledger.open_lots {
            if crate::occ::parse_occ(sym).is_none() {
                continue;
            }
            for l in lots {
                let Some(cid) = l.combo_id else { continue };
                let e = legs.entry(cid).or_default().entry(sym.clone()).or_insert((0.0, 0.0, 0.0));
                e.0 += l.qty;
                e.1 += l.qty * l.price * l.multiplier;
                e.2 += l.qty * mark.get(sym.as_str()).copied().unwrap_or(f64::NAN) * l.multiplier;
            }
        }
        let orders = O::parse_orders(&a.bundle.orders);
        let mut out = HashMap::new();
        for (cid, lg) in legs {
            let lg: Vec<(String, f64, f64, f64)> = lg.into_iter().filter(|(_, v)| v.0.abs() > 1e-9).map(|(k, v)| (k, v.0, v.1, v.2)).collect();
            if lg.is_empty() {
                continue;
            }
            let basis: f64 = lg.iter().map(|x| x.2).sum();
            let value: f64 = lg.iter().map(|x| x.3).sum();
            let qty: Vec<(String, f64)> = lg.iter().map(|x| (x.0.clone(), x.1)).collect();
            let (id, refs, fill) = closer(&qty, basis, value, &orders, &cid.to_string());
            let if_filled = fill.map(|f| f.0);
            if !id.is_empty() {
                out.insert(cid, (id, refs.to_string(), if_filled));
            }
        }
        out
    }
}

/// What the change-price popup needs to close `legs` (symbol, signed qty) at a chosen profit against
/// `basis`: the open limit order that closes exactly these legs (to change it), or a new closing order
/// ("new:<key>", legs reversed). Also, for an existing order, the P&L if it fills and its net price.
pub fn closer(legs: &[(String, f64)], basis: f64, value: f64, orders: &[O::PendingOrder], key: &str) -> (String, String, Option<(f64, f64)>) {
    let und = crate::occ::underlying_of(&legs[0].0);
    let leg_txt = legs.iter().map(|x| format!("{}{} {}", if x.1 > 0.0 { "+" } else { "" }, util::g(x.1), super::leg(&x.0))).collect::<Vec<_>>().join(", ");
    let exact = orders.iter().find(|o| {
        o.order_type == "limit"
            && o.legs.len() == legs.len()
            && o.legs.iter().all(|l| legs.iter().any(|x| x.0 == l.symbol && (l.ratio * o.units + x.1).abs() < 1e-9))
    });
    let mut refs = json!({"basis": jr(basis, 2), "label": format!("{} close · {}", und, leg_txt)});
    if !value.is_nan() {
        refs["target"] = jr(value - basis, 2);
    }
    let fill = exact.and_then(|o| o.net.map(|n| (-n * o.units * 100.0 - basis, n)));
    let id = match exact {
        Some(o) => {
            refs["scale"] = jf(o.units * 100.0);
            o.id.clone()
        }
        None => {
            // open orders that already close some of these legs: Alpaca may refuse a second one
            let overlap: Vec<String> = orders
                .iter()
                .filter(|o| o.legs.iter().any(|l| legs.iter().any(|x| x.0 == l.symbol && (l.ratio > 0.0) != (x.1 > 0.0))))
                .map(|o| format!("{} at {}", o.legs_label(), o.net.map(|n| format!("${:.2} {}", n.abs(), if n < 0.0 { "credit" } else { "debit" })).unwrap_or_else(|| "market".into())))
                .collect();
            if !overlap.is_empty() {
                refs["overlap"] = json!(overlap.join("; "));
            }
            let q = legs.iter().map(|x| x.1.abs().round() as u64).fold(0, gcd).max(1) as f64;
            refs["scale"] = jf(q * 100.0);
            refs["new"] = json!({"qty": q, "legs": legs.iter().map(|x| json!({
                "symbol": x.0, "side": if x.1 > 0.0 { "sell" } else { "buy" },
                "position_intent": if x.1 > 0.0 { "sell_to_close" } else { "buy_to_close" },
                "ratio_qty": (x.1.abs() / q).round()})).collect::<Vec<_>>()});
            let k: String = key.chars().filter(|c| c.is_ascii_digit()).take(9).collect();
            format!("new:{}", if k.is_empty() { "0".into() } else { k })
        }
    };
    (id, refs.to_string(), fill)
}

fn whole_cost(units: &[crate::interpret::Unit]) -> f64 {
    units.iter().map(|u| u.cost).sum()
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}
