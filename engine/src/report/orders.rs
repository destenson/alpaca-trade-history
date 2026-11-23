//! Orders, Positions and Notes tabs, and the re-pricing input card.

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{json, Value};

use super::{dollars_list, jf, jr, jvec, leg, lim, pn, slug, spot_from, table, when, Cell, Report, Tbl, GAIN, LOSS, NEUTRAL, S};
use crate::analysis::Recon;
use crate::fillodds::fill_odds;
use crate::orders::{self as O, PendingOrder};
use crate::reprice as rp;
use crate::util::{self, commas, esc, interp, money, num, pct, pyfmt_pct};

fn px(v: Option<f64>, est: bool) -> String {
    match v {
        None => "–".into(),
        Some(v) if v.is_nan() => "–".into(),
        Some(v) => format!(
            "${} {}{}",
            commas(v.abs(), 2),
            if v > 0.0 { "debit" } else if v < 0.0 { "credit" } else { "even" },
            if est { " (mkt est.)" } else { "" }
        ),
    }
}

fn rr(o: &PendingOrder) -> f64 {
    if o.max_gain.is_infinite() || o.max_loss.is_infinite() || o.max_loss == 0.0 || o.max_loss.is_nan() {
        return f64::NAN;
    }
    o.max_gain / -o.max_loss
}

/// Strategy; orders on contracts with no quotes yet carry a hover note saying how they are priced.
fn strat(o: &PendingOrder) -> Cell {
    if !o.no_market {
        return Cell::s(&o.strategy);
    }
    let how = match o.legs.iter().find_map(|l| l.model.as_ref()).filter(|_| o.rv_priced) {
        Some(m) => format!("Estimates use {}'s own {} realized volatility from its recent 5-minute prices, in place of option-implied volatility.", o.underlying, pyfmt_pct(m.iv, 0, false)),
        None => "Nothing to estimate a value from, so only the payoff at your limit is shown.".into(),
    };
    Cell::Tip(o.strategy.clone(), format!("No quotes yet: these contracts haven't traded. {}", how))
}

fn name(o: &PendingOrder) -> String {
    match o.expiry {
        Some(e) => format!("{} {} {}", o.underlying, o.strategy, e.format("%m/%d")),
        None => format!("{} {}", o.underlying, o.strategy),
    }
}

fn edge_est(o: &PendingOrder) -> f64 {
    match (o.est_open, o.net) {
        (Some(e), Some(n)) if !o.net_is_estimate => (e - n) * o.units * o.mult(),
        _ => f64::NAN,
    }
}

fn fill_text(est_unit: Option<f64>, net: f64) -> String {
    let Some(e) = est_unit else { return String::new() };
    let d = e - net;
    if d <= 0.005 {
        "at or through your limit: likely to fill near the open".into()
    } else {
        format!("your limit is ${} better than the estimate: fills only if prices move your way", commas(d, 2))
    }
}

fn bes(v: &[f64]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        dollars_list(v)
    }
}

/// Python round() to an integer (half to even).
fn pyround(x: f64) -> f64 {
    x.round_ties_even()
}

impl<'a> Report<'a> {
    pub fn orders(&mut self) {
        let a = self.a;
        let mut allo = O::parse_orders(&a.bundle.orders);
        if allo.is_empty() {
            return;
        }
        let today = self.today();
        let snaps = &a.bundle.option_snapshots;
        let market = self.market().cloned().unwrap();
        O::evaluate(&mut allo, &a.ledger, &a.open_positions, snaps, &|u| spot_from(a, u), today, Some(&market));
        let skipped_close = allo.iter().filter(|o| o.is_option && o.effect == "close").count();
        let skipped_stock = allo.iter().filter(|o| !o.is_option).count();
        let mut skipped = Vec::new();
        if skipped_close > 0 {
            skipped.push(format!("{} closing option order{}", skipped_close, if skipped_close != 1 { "s" } else { "" }));
        }
        if skipped_stock > 0 {
            skipped.push(format!("{} stock order{}", skipped_stock, if skipped_stock != 1 { "s" } else { "" }));
        }
        let skip_note = if skipped.is_empty() {
            String::new()
        } else {
            format!("<p class=\"note\">Not projected here: {} (take-profits and exits on positions you already hold).</p>", skipped.join(" and "))
        };
        let mut opening: Vec<PendingOrder> = allo.into_iter().filter(|o| o.is_option && o.effect != "close").collect();
        if opening.is_empty() {
            self.section("orders", "Orders", format!("<p class=\"empty\">No open option strategy orders.</p>{}", skip_note));
            return;
        }
        opening.sort_by(|x, y| {
            (x.expiry.unwrap_or(NaiveDate::MAX), x.underlying.clone(), x.submitted.map(|s| s.timestamp_micros()).unwrap_or(0))
                .cmp(&(y.expiry.unwrap_or(NaiveDate::MAX), y.underlying.clone(), y.submitted.map(|s| s.timestamp_micros()).unwrap_or(0)))
        });
        for o in opening.iter_mut() {
            o.fill = if o.model.is_some() { fill_odds(o, &market, 0.0) } else { None };
        }
        let priced: Vec<usize> = (0..opening.len()).filter(|&i| opening[i].curves.is_some()).collect();
        let w = when(&market);

        // ---- tiles
        let net: f64 = opening.iter().filter_map(|o| o.net.map(|n| n * o.units * o.mult())).sum();
        // a worst case that is still a gain (e.g. an inverted strangle bought at $0) counts as no loss
        let fin_loss: Vec<f64> = priced.iter().map(|&i| opening[i].max_loss.min(0.0)).filter(|v| !v.is_infinite()).collect();
        let fin_gain: Vec<f64> = priced.iter().map(|&i| opening[i].max_gain).filter(|v| !v.is_infinite()).collect();
        let n_unl = priced.iter().filter(|&&i| opening[i].max_loss.is_infinite()).count();
        let pops: Vec<f64> = priced.iter().filter_map(|&i| opening[i].pop).collect();
        let edge: Vec<f64> = opening.iter().filter(|o| o.mid_now.is_some() && o.net.is_some() && !o.net_is_estimate).map(|o| o.mid_now.unwrap() - o.net.unwrap()).collect();
        let nearest = opening.iter().filter_map(|o| o.expiry).min();
        let und_set: Vec<&str> = {
            let mut v: Vec<&str> = opening.iter().map(|o| o.underlying.as_str()).collect();
            v.sort();
            v.dedup();
            v
        };
        let mut stale: Vec<&rp::Underlying> = market.underlyings.values().filter(|u| u.stale() && und_set.contains(&u.symbol.as_str())).collect();
        stale.sort_by(|x, y| y.mv().abs().partial_cmp(&x.mv().abs()).unwrap_or(std::cmp::Ordering::Equal));
        let sfl: f64 = fin_loss.iter().sum();
        let sfg: f64 = fin_gain.iter().sum();
        let any_fill = opening.iter().any(|o| o.fill.is_some());
        let mut tiles: Vec<(String, String, String)> = vec![
            ("Open strategy orders".into(), opening.len().to_string(),
                format!("{} underlyings{}{}", und_set.len(), nearest.map(|n| format!(" · first expiry {} ({} d)", n.format("%b %d"), (n - today).num_days())).unwrap_or_default(),
                    match opening.iter().filter(|o| o.no_market).count() {
                        0 => String::new(),
                        n => format!(" · {} on contracts with no quotes yet", n),
                    })),
            ("If all fill".into(), px(Some(pyround(net)), false).replace(".00", ""), "net premium at your limit prices, before fees".into()),
            ("Combined max loss".into(), if fin_loss.is_empty() { "–".into() } else { money(sfl, false, 0) },
                format!("{} · each order's own worst case", if n_unl > 0 { format!("+ {} undefined-risk", n_unl) } else { "every order is defined-risk".into() })),
            ("Combined max profit".into(), if fin_gain.is_empty() { "–".into() } else { money(sfg, false, 0) },
                if !fin_loss.is_empty() && sfl != 0.0 { format!("reward/risk {:.2}", sfg / -sfl) } else { String::new() }),
            ("Median prob. of profit".into(), if pops.is_empty() { "–".into() } else { pct(util::median(&pops), false, 0) }, "at first expiry, from implied vol".into()),
            ("Limits vs mid".into(), if edge.is_empty() { "–".into() } else { format!("{}/{}", edge.iter().filter(|e| **e >= -0.005).count(), edge.len()) },
                "priced at or better than the quoted mid (less likely to fill)".into()),
            ("Likely to fill".into(),
                if any_fill { format!("{}/{}", opening.iter().filter(|o| o.fill.as_ref().and_then(|f| f.h[0]).unwrap_or(0.0) >= 0.5).count(), opening.iter().filter(|o| o.fill.is_some()).count()) } else { "–".into() },
                "<a href=\"#fillodds\" class=\"tilelink\">50%+ chance in the next session: see which</a>".into()),
            ("Expected fills".into(),
                if any_fill { format!("{:.1}", opening.iter().filter_map(|o| o.fill.as_ref()).map(|f| f.p_fill).sum::<f64>()) } else { "–".into() },
                if any_fill { format!("of {} · {:.1} expected to fill and end in profit", opening.iter().filter(|o| o.fill.is_some()).count(),
                    opening.iter().filter_map(|o| o.fill.as_ref()).map(|f| f.fill_and_profit).sum::<f64>()) } else { String::new() }),
        ];
        if !stale.is_empty() {
            tiles.insert(1, ("Moved since quotes".into(), stale.len().to_string(),
                stale.iter().map(|u| format!("{} {}", u.symbol, pyfmt_pct(u.mv(), 1, true))).collect::<Vec<_>>().join(", ")));
        }
        let mut parts = vec![Self::tiles(&tiles)];

        let pu: Vec<String> = {
            let mut v: Vec<String> = priced.iter().map(|&i| opening[i].underlying.clone()).collect();
            v.sort();
            v.dedup();
            v
        };
        parts.push(self.rp_card(&pu, "Edit a price or apply an IV change (e.g. −30 for an earnings vol crush) and every estimate, table cell and chart for that underlying updates, here and on the Options tab."));

        // ---- risk / reward overview
        if !priced.is_empty() {
            let ys: Vec<String> = priced.iter().enumerate().map(|(k, &i)| format!("{} · #{}", name(&opening[i]), k + 1)).collect();
            let gain: Vec<Value> = priced.iter().map(|&i| if opening[i].max_gain.is_infinite() { Value::Null } else { jr(opening[i].max_gain, 2) }).collect();
            let loss: Vec<Value> = priced.iter().map(|&i| if opening[i].max_loss.is_infinite() { Value::Null } else { jr(opening[i].max_loss.min(0.0), 2) }).collect();
            let cd: Vec<Value> = priced
                .iter()
                .map(|&i| {
                    let o = &opening[i];
                    let fp = |v: Option<f64>| v.map(|p| pyfmt_pct(p, 0, false)).unwrap_or_else(|| "–".into());
                    let fill = o.fill.as_ref();
                    json!([o.legs_label(), px(o.net, o.net_is_estimate), fp(o.pop), lim(o.max_gain), super::lim_loss(o.max_loss),
                           fp(fill.map(|f| f.p_fill)), fp(fill.and_then(|f| f.profit_if_filled)), fp(fill.map(|f| f.fill_and_profit))])
                })
                .collect();
            let ht = "<b>%{y}</b><br>%{customdata[0]}<br>limit %{customdata[1]}<br>max profit %{customdata[3]} · max loss %{customdata[4]}<br>prob. of profit %{customdata[2]} now · %{customdata[6]} if filled<br>chance of filling %{customdata[5]} · fill and profit %{customdata[7]}<extra></extra>";
            let mut data = vec![
                json!({"type": "bar", "orientation": "h", "y": ys, "x": gain, "name": "Max profit", "marker": {"color": GAIN}, "customdata": cd, "hovertemplate": ht}),
                json!({"type": "bar", "orientation": "h", "y": ys, "x": loss, "name": "Max loss", "marker": {"color": LOSS}, "customdata": cd, "hovertemplate": ht}),
            ];
            if !pops.is_empty() {
                data.push(json!({"type": "scatter", "mode": "markers", "y": ys, "xaxis": "x2", "name": "Prob. of profit",
                    "x": priced.iter().map(|&i| opening[i].pop.map(jf).unwrap_or(Value::Null)).collect::<Vec<_>>(),
                    "marker": {"color": "var:ink", "symbol": "diamond", "size": 9}, "customdata": cd, "hovertemplate": ht}));
            }
            if any_fill {
                let pf = |g: &dyn Fn(&crate::fillodds::FillOdds) -> f64| priced.iter().map(|&i| opening[i].fill.as_ref().map(|f| jf(g(f))).unwrap_or(Value::Null)).collect::<Vec<_>>();
                data.push(json!({"type": "scatter", "mode": "markers", "y": ys, "xaxis": "x2", "name": "Chance of filling", "x": pf(&|f| f.p_fill),
                    "marker": {"color": "var:ink", "symbol": "circle-open", "size": 10, "line": {"width": 2}}, "customdata": cd, "hovertemplate": ht}));
                data.push(json!({"type": "scatter", "mode": "markers", "y": ys, "xaxis": "x2", "name": "Fill and profit", "x": pf(&|f| f.fill_and_profit),
                    "marker": {"color": "var:ink", "symbol": "x-thin-open", "size": 9, "line": {"width": 2}}, "customdata": cd, "hovertemplate": ht}));
            }
            parts.push(self.fig("f_ord_rr", data, json!({"barmode": "relative", "margin": {"l": 230, "t": 36}, "yaxis": {"autorange": "reversed"},
                "xaxis": {"tickprefix": "$", "title": {"text": "max loss ← at expiration → max profit"}},
                "xaxis2": {"overlaying": "x", "side": "top", "range": [0, 1], "tickformat": ".0%", "showgrid": false, "title": {"text": "chance"}},
                "legend": {"orientation": "h", "y": -0.12, "yanchor": "top"}, "showlegend": true}),
                (34 * priced.len() as i64 + 120).max(280), "", "Risk and reward if each order fills",
                "At your limit price, held to expiration, fees excluded. Undefined-risk sides are left off the bars (see the table). Top axis, market-implied from the legs' implied vol (lognormal, no drift), estimates rather than forecasts: ◆ chance of profit from today's price; ○ chance the order fills (before it expires; this session for DAY orders); ✕ chance it fills <i>and</i> ends in profit. ✕ is usually well below ◆ × ○: a limit better than fair value tends to fill after the price has moved against the trade, so a filled order's odds are worse than today's.", true));
        }

        // ---- tables: what you pay (entry) and what you can make (if filled); a few columns in both
        let est_col = if market.is_open { "Est. now" } else { "Est. at open" };
        let exp_cell = |o: &PendingOrder| match o.expiry {
            Some(e) => {
                let d = (e - today).num_days();
                let date = if chrono::Datelike::year(&e) == chrono::Datelike::year(&today) { e.format("%m/%d").to_string() } else { e.format("%m/%d/%y").to_string() };
                Cell::T(format!("{} · {}d", date, d), d as f64)
            }
            None => Cell::s("–"),
        };
        let mut entry = Tbl::new(&["#", "Underlying", "Strategy", "Legs", "DTE", "Limit", "Mid (quoted)", "Natural", "Edge vs mid", est_col, "Edge vs est.", "TIF", "Placed"])
            .f("Edge vs mid", super::fm(true, 2))
            .f("Edge vs est.", super::fm(true, 2));
        let mut payoff = Tbl::new(&["#", "Underlying", "Strategy", "Legs", "Expires", "Limit", "Max profit", "Max loss", "Reward/risk", "Breakevens", "Prob. profit", "Fill", "Fill & profit"])
            .f("Max profit", super::flim())
            .f("Max loss", super::flim_loss())
            .f("Reward/risk", super::fnum(2))
            .f("Prob. profit", super::fp(false, 0))
            .f("Fill", super::fp(false, 0))
            .f("Fill & profit", super::fp(false, 0));
        for (k, o) in opening.iter().enumerate() {
            let em = match (o.mid_now, o.net) {
                (Some(m), Some(n)) if !o.net_is_estimate => (m - n) * o.units * o.mult(),
                _ => f64::NAN,
            };
            let head = |exp: Cell| vec![Cell::I(k as i64 + 1), Cell::s(&o.underlying), strat(o), Cell::s(o.legs_label()), exp, Cell::s(px(o.net, o.net_is_estimate))];
            let dte = match o.expiry {
                Some(e) => {
                    let d = (e - today).num_days();
                    Cell::T(format!("{}d", d), d as f64)
                }
                None => Cell::s("–"),
            };
            let placed = match o.submitted {
                Some(s) => Cell::T(util::to_ny(&s).format("%m/%d %H:%M").to_string(), s.timestamp() as f64),
                None => Cell::s("–"),
            };
            let mut r = head(dte);
            r.extend([
                Cell::s(px(o.mid_now, false)),
                Cell::s(px(o.natural, false)),
                Cell::F(em),
                Cell::s(px(o.est_open, false)),
                Cell::F(edge_est(o)),
                Cell::s(o.tif.to_uppercase()),
                placed,
            ]);
            entry.push(r);
            let mut r = head(exp_cell(o));
            r.extend([
                Cell::F(o.max_gain),
                Cell::F(o.max_loss),
                Cell::F(rr(o)),
                Cell::s(if o.breakevens.is_empty() { "–".to_string() } else { dollars_list(&o.breakevens) }),
                Cell::F(o.pop.unwrap_or(f64::NAN)),
                Cell::F(o.fill.as_ref().map(|f| f.p_fill).unwrap_or(f64::NAN)),
                Cell::F(o.fill.as_ref().map(|f| f.fill_and_profit).unwrap_or(f64::NAN)),
            ]);
            payoff.push(r);
        }
        let legs_note = "<b>Legs</b>: blue = buy, red = sell, a quantity only when it isn't 1, each expiry written once.";
        parts.push(format!(
            "<figure class=\"card wide\"><h3>Entry: prices and estimates</h3><p class=\"sub\">Net price per spread: debit you pay or credit you receive. Mid/natural are the last quotes (stale while the market is closed); <b>Est.</b> is the model value at {} from the re-pricing inputs above. <b>Edge</b> = dollars your limit is better than the mid / estimate (blue: a better price, but less likely to fill; red: you are paying up). {}</p>{}</figure>",
            w,
            legs_note,
            table(&entry, "t_orders", &["Edge vs mid", "Edge vs est."])
        ));
        parts.push(format!(
            "<figure class=\"card wide\"><h3>If filled: payoff and odds</h3><p class=\"sub\">Each order held to expiration after filling at your limit, fees excluded. <b>Prob. profit</b> is the market-implied chance the underlying finishes past breakeven at the first expiry (lognormal at the legs' IV, no drift). <b>Fill</b> is the chance it fills, from the Chance of filling simulation (before the order expires; this session for DAY orders); <b>Fill &amp; profit</b> is the chance it fills <i>and</i> ends in profit (hover the chart above for the chance of profit once filled). {}{}</p>{}</figure>",
            if opening.iter().any(|o| o.no_market) { "A dotted underline on a strategy means a note: hover it. " } else { "" },
            legs_note,
            table(&payoff, "t_payoff", &[])
        ));

        // ---- chance of filling
        let hz = ["<1d", "<2d", "<3d", "<5d", "<8d"];
        let mut cols: Vec<&str> = vec!["#", "Underlying", "Strategy", "Legs", "DTE", "TIF", "Limit", est_col];
        let dte_cell = |o: &PendingOrder| match o.expiry {
            Some(e) => {
                let d = (e - today).num_days();
                Cell::T(format!("{}d", d), d as f64)
            }
            None => Cell::s("–"),
        };
        cols.extend(hz.iter());
        cols.extend(["Before expiry", "Good until"]);
        let pf = |c: &Cell| {
            if let Cell::S(t) = c {
                return t.clone();
            }
            let v = c.f();
            if v.is_nan() {
                "–".to_string()
            } else if v > 0.0 && v < 0.005 {
                "<1%".into()
            } else if v > 0.995 && v < 1.0 {
                ">99%".into()
            } else {
                pyfmt_pct(v, 0, false)
            }
        };
        let mut ft = Tbl::new(&cols);
        for h in hz.iter().chain(["Before expiry"].iter()) {
            ft = ft.f(h, pf);
        }
        for (k, o) in opening.iter().enumerate() {
            if o.unpriced() {
                let mut row = vec![Cell::I(k as i64 + 1), Cell::s(&o.underlying), strat(o), Cell::s(o.legs_label()), dte_cell(o), Cell::s(o.tif.to_uppercase()),
                                   Cell::s(px(o.net, o.net_is_estimate)), Cell::s("–")];
                row.push(Cell::s("no quotes yet"));
                row.extend((0..5).map(|_| Cell::N));
                let good_until = if o.tif.eq_ignore_ascii_case("gtc") {
                    let cap = o.submitted.map(|s| util::ny_date(&(s + chrono::Duration::days(crate::fillodds::GTC_DAYS))));
                    match (o.expiry, cap) {
                        (Some(e), Some(c)) => super::isod(e.min(c)),
                        (Some(e), None) => super::isod(e),
                        _ => "GTC".into(),
                    }
                } else {
                    "end of session".into()
                };
                row.push(Cell::s(good_until));
                ft.push(row);
                continue;
            }
            let Some(f) = &o.fill else { continue };
            let mut row = vec![
                Cell::I(k as i64 + 1),
                Cell::s(&o.underlying),
                strat(o),
                Cell::s(o.legs_label()),
                dte_cell(o),
                Cell::s(o.tif.to_uppercase()),
                Cell::s(px(o.net, o.net_is_estimate)),
                Cell::s(px(o.est_open, false)),
            ];
            for h in f.h.iter() {
                row.push(Cell::F(h.unwrap_or(f64::NAN)));
            }
            row.push(Cell::F(f.expiry.unwrap_or(f64::NAN)));
            row.push(Cell::s(if f.gtc { super::isod(f.last_day) } else { "end of session".into() }));
            ft.push(row);
        }
        if !ft.rows.is_empty() {
            parts.push(format!(
                "<figure class=\"card wide\" id=\"fillodds\"><h3>Chance of filling</h3><p class=\"sub\">Chance the estimated value of each order reaches your limit within the next 1, 2, 3, 5 and 8 trading sessions (DAY orders only have the session they live for). GTC orders also show the chance before they expire: the first option expiration, or Alpaca's 90-day GTC limit. Simulated from the re-pricing price at the legs' implied volatility, re-pricing the spread every half hour of trading; it measures the fair value touching your limit, so treat it as a guide: actual fills also depend on the bid/ask and market makers.</p>{}</figure>",
                table(&ft, "t_fill", &[])
            ));
        }

        // ---- payoff explorer
        let mut opts = Vec::new();
        let mut views = Vec::new();
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for &i in &priced {
            let u = &opening[i].underlying;
            match groups.iter_mut().find(|g| &g.0 == u) {
                Some(g) => g.1.push(i),
                None => groups.push((u.clone(), vec![i])),
            }
        }
        // same order as the # column
        groups.sort_by_key(|g| *g.1.iter().min().unwrap());
        let mut items: Vec<(bool, String, Vec<usize>)> = Vec::new();
        for (und, os) in &groups {
            if os.len() > 1 {
                items.push((true, und.clone(), os.clone()));
            }
            for &i in os {
                items.push((false, und.clone(), vec![i]));
            }
        }
        let mut group_open: Option<String> = None;
        for (j, (all, und, os)) in items.iter().enumerate() {
            let vid = format!("ov_{}", j);
            let (label, cur, mut stats, comb_ml, comb_mg, comb_be, sp, exp_lbl, model, net_total, um, rows_k, est_total);
            let no_market = os.iter().all(|&i| opening[i].unpriced());
            if *all {
                let refs: Vec<&PendingOrder> = os.iter().map(|&i| &opening[i]).collect();
                let Some(d) = O::combine(&refs, &a.open_positions, snaps, today, Some(&market)) else { continue };
                label = format!("{}: all {} orders together", und, os.len());
                stats = format!("{} orders · net {} · max profit {} · max loss {} · breakevens {}", os.len(), px(Some(d.net), false), lim(d.max_gain), super::lim_loss(d.max_loss), bes(&d.breakevens));
                comb_ml = d.curves.comb_max_loss;
                comb_mg = d.curves.comb_max_gain;
                comb_be = d.curves.comb_breakevens.clone();
                sp = d.spot;
                exp_lbl = os.iter().filter_map(|&i| opening[i].expiry).min();
                model = d.model.clone();
                net_total = d.net;
                um = None;
                rows_k = vec![];
                est_total = d.est_open;
                cur = d.curves;
            } else {
                let o = &opening[os[0]];
                let k = os[0] + 1;
                label = format!("#{} {} @ {}", k, name(o), px(o.net, o.net_is_estimate));
                cur = o.curves.clone().unwrap();
                stats = format!(
                    "{} · limit {} (quoted mid {}, natural {}) · max profit {} · max loss {} · reward/risk {} · breakevens {}{}",
                    o.legs_label(),
                    px(o.net, o.net_is_estimate),
                    px(o.mid_now, false),
                    px(o.natural, false),
                    lim(o.max_gain),
                    super::lim_loss(o.max_loss),
                    num(rr(o), 2),
                    bes(&o.breakevens),
                    o.pop.map(|p| format!(" · prob. of profit ~{}", pyfmt_pct(p, 0, false))).unwrap_or_default()
                );
                comb_ml = cur.comb_max_loss;
                comb_mg = cur.comb_max_gain;
                comb_be = cur.comb_breakevens.clone();
                sp = o.spot.unwrap();
                exp_lbl = o.expiry;
                model = o.model.clone();
                let u = o.units * o.mult();
                um = Some(u);
                rows_k = vec![k];
                net_total = o.net.unwrap() * u;
                est_total = o.est_open.map(|e| e * u);
            }
            if model.is_some() {
                let eu = match (um, est_total) {
                    (Some(u), Some(e)) => Some(e / u),
                    _ => None,
                };
                let est_txt = match um {
                    Some(u) => format!("Estimated value at {}: <b>{}</b> vs your limit {} · {}", w, px(eu, false), px(Some(net_total / u), false), fill_text(eu, net_total / u)),
                    None => format!("Estimated value at {}: <b>{}</b> vs {} at your limits", w, px(est_total, false), px(Some(net_total), false)),
                };
                stats.push_str(&format!("<br><span class=\"rp-est\" id=\"est_{}\">{}</span>", vid, est_txt));
            }
            if no_market {
                stats.push_str("<br>No quotes yet on these contracts: the payoff at expiration is shown; the estimated value curve appears once they trade.");
            }
            if cur.combined.is_some() {
                stats.push_str(&format!(
                    "<br>With your current {} position: max loss after fill {} · max profit {} · breakevens {}",
                    esc(und),
                    super::lim_loss(comb_ml),
                    lim(comb_mg),
                    bes(&comb_be)
                ));
            }
            let open_name = if model.is_some() { if market.is_open { "Order now (est.)" } else { "Order at next open (est.)" } } else { "Order today (at IV)" };
            let mut data = vec![json!({"type": "scatter", "mode": "lines", "name": "Order at expiration", "x": super::jraw(&cur.x), "y": super::jraw(&cur.expiry), "line": {"color": S[0], "width": 2.5}})];
            if !no_market {      // with no quotes, a value before expiration would rest on a guessed volatility
                data.push(json!({"type": "scatter", "mode": "lines", "name": open_name, "x": super::jraw(&cur.x), "y": super::jraw(&cur.today), "line": {"color": S[0], "width": 1.5, "dash": "dot"}}));
            }
            let mut t_mark = Value::Null;
            let mut t_comb = Value::Null;
            if model.is_some() {
                let y0 = interp(sp, &cur.x, &cur.today);
                t_mark = json!(data.len());
                data.push(json!({"type": "scatter", "mode": "markers", "name": "Estimated P&L at that price", "x": [jf(sp)], "y": [jr(y0, 2)],
                    "marker": {"color": S[0], "size": 9, "symbol": "circle-open", "line": {"width": 2}}, "hovertemplate": "est. P&L %{y:$,.2f}<extra></extra>"}));
            }
            if let (Some(pos), Some(comb)) = (&cur.position, &cur.combined) {
                data.push(json!({"type": "scatter", "mode": "lines", "name": "Current position at expiration", "x": super::jraw(&cur.x), "y": super::jraw(pos), "line": {"color": NEUTRAL, "width": 1.5}}));
                data.push(json!({"type": "scatter", "mode": "lines", "name": "Position + order at expiration", "x": super::jraw(&cur.x), "y": super::jraw(comb), "line": {"color": "var:ink", "width": 2, "dash": "dash"}}));
                if let (Some(m), Some(ct)) = (&model, &cur.combined_today) {
                    if m.pos.is_some() {
                        t_comb = json!(data.len());
                        data.push(json!({"type": "scatter", "mode": "lines", "name": "Position + order at next open (est.)", "x": super::jraw(&cur.x), "y": super::jraw(ct), "line": {"color": "var:ink", "width": 1.2, "dash": "dot"}}));
                    }
                }
            }
            let shapes = json!([{"type": "line", "x0": sp, "x1": sp, "yref": "paper", "y0": 0, "y1": 1, "line": {"color": "var:muted", "width": 1, "dash": "dot"}},
                                {"type": "line", "xref": "paper", "x0": 0, "x1": 1, "y0": 0, "y1": 0, "line": {"color": "var:axis", "width": 1}}]);
            let mu = market.underlyings.get(und);
            let spot_lbl = if mu.map(|u| u.stale()).unwrap_or(false) { "latest" } else { "spot" };
            let fid = if *all {
                format!("f_ord_{}", slug(&[und, "all"]))
            } else {
                let o = &opening[os[0]];
                let key = if o.id.is_empty() { o.legs_label() } else { o.id.clone() };
                format!("f_ord_{}", slug(&[und, &key]))
            };
            let xt = format!("{} price at expiration{}", und, exp_lbl.map(|e| format!(" ({})", e.format("%b %d"))).unwrap_or_default());
            let fig = self.fig(&fid, data, json!({"shapes": shapes, "hovermode": "x unified", "showlegend": true,
                "legend": {"orientation": "h", "y": -0.22, "yanchor": "top"},
                "xaxis": {"tickprefix": "$", "title": {"text": xt}}, "yaxis": {"tickprefix": "$"},
                "annotations": [{"x": sp, "yref": "paper", "y": 1, "text": format!("{} ${}", spot_lbl, commas(sp, 2)), "showarrow": false, "yanchor": "bottom", "font": {"size": 11}}]}),
                380, "", "", "", false);
            if let Some(m) = &model {
                self.rp_views.push(json!({"kind": "order", "fid": fid, "und": und, "x": super::jraw(&cur.x), "order": m.order.to_json(),
                    "pos": if t_comb.is_null() { Value::Null } else { m.pos.as_ref().map(|p| p.to_json()).unwrap_or(Value::Null) },
                    "net": jf(net_total), "um": um.map(jf).unwrap_or(Value::Null), "t_open": 1, "t_mark": t_mark, "t_comb": t_comb,
                    "est": format!("est_{}", vid), "rows": rows_k}));
            }
            if group_open.as_deref() != Some(und.as_str()) {
                opts.push(format!("{}<optgroup label=\"{}\">", if group_open.is_some() { "</optgroup>" } else { "" }, esc(und)));
                group_open = Some(und.clone());
            }
            opts.push(format!("<option value=\"{}\">{}</option>", vid, esc(&label)));
            views.push(format!("<div class=\"interp-view\" id=\"{}\"{}><p class=\"sub\">{}</p>{}</div>", vid, if views.is_empty() { "" } else { " hidden" }, stats, fig));
        }
        if group_open.is_some() {
            opts.push("</optgroup>".into());
        }
        if !views.is_empty() {
            parts.push(format!(
                "<figure class=\"card wide\"><h3>Payoff if filled</h3><label class=\"pick\">Order <select class=\"interp\" data-box=\"ob\">{}</select></label><div id=\"ob\">{}</div><p class=\"note\">P&amp;L at your limit price, fees excluded. Expiration curves value every leg at intrinsic (later-dated legs of calendars/diagonals are approximated). The dotted curve is the estimated value at {} from the re-pricing inputs. Where you already hold the underlying or options on it, the dashed line shows the whole position after the fill.</p></figure>",
                opts.concat(),
                views.concat(),
                w
            ));
        }
        parts.push(skip_note);
        self.section("orders", "Orders", parts.concat());
    }

    /// Per-underlying re-pricing inputs. Rows for the same underlying stay in sync across tabs.
    pub fn rp_card(&mut self, unds: &[String], how: &str) -> String {
        let Some(market) = self.market().cloned() else { return String::new() };
        let mut unds: Vec<&String> = unds.iter().collect();
        unds.sort_by(|x, y| {
            let k = |u: &String| market.underlyings.get(u).map(|m| -m.mv().abs()).unwrap_or(0.0);
            k(x).partial_cmp(&k(y)).unwrap_or(std::cmp::Ordering::Equal).then(x.cmp(y))
        });
        let mut rows = Vec::new();
        for und in unds {
            let Some(u) = market.underlyings.get(und) else { continue };
            let Some(spot) = u.spot.filter(|s| *s != 0.0) else { continue };
            let lt = match (u.last, u.last_time) {
                (Some(l), Some(t)) => {
                    let n = util::to_ny(&t);
                    format!(
                        "${} · {} sh · {} {}{}{}",
                        commas(l, 2),
                        u.last_size.map(|s| commas(s, 0)).unwrap_or_else(|| "nan".into()),
                        n.format("%a"),
                        util::clock(&n),
                        if u.last_feed.is_empty() || u.last_feed == "primary" { String::new() } else { format!(" ({})", u.last_feed) },
                        if u.last_ext { " · after hours" } else { "" }
                    )
                }
                _ => "–".into(),
            };
            let qt = u.quote_time.map(|t| {
                let n = util::to_ny(&t);
                format!("{} {}", n.format("%a"), util::clock(&n))
            });
            let sp4 = util::g(util::round_dp(spot, 4));
            rows.push(format!(
                "<tr class=\"{}\" data-und=\"{}\"><td><b>{}</b></td><td>{}</td><td>{}</td><td class=\"rp-move\">{}</td><td>{}</td><td><input class=\"rp-spot\" type=\"number\" inputmode=\"decimal\" min=\"0\" step=\"any\" value=\"{}\" data-default=\"{}\" data-ref=\"{:.6}\" aria-label=\"{} price for estimates\"></td><td><input class=\"rp-iv\" type=\"number\" inputmode=\"decimal\" step=\"5\" min=\"-90\" max=\"300\" value=\"0\" aria-label=\"{} IV change in percent\"> %</td><td><button type=\"button\" class=\"rp-reset\">Reset</button></td></tr>",
                if u.stale() { "stale" } else { "" },
                esc(und),
                super::ticker(und),
                super::cur(&money(u.refp.unwrap_or(f64::NAN), false, 2)),
                super::cur(&lt),
                pct(u.mv(), true, 1),
                qt.unwrap_or_else(|| "–".into()),
                sp4,
                sp4,
                u.refp.filter(|r| *r != 0.0).unwrap_or(spot),
                esc(und),
                esc(und)
            ));
        }
        if rows.is_empty() {
            return String::new();
        }
        let w = when(&market);
        let status = if market.is_open {
            "The market is open: option quotes are live, so the inputs are what-ifs.".to_string()
        } else {
            format!("The market is closed. Option quotes are from the last session; the underlying keeps trading after hours and overnight. Estimates are for {}.", w)
        };
        let heads: String = ["Underlying", "Quotes reflect", "Latest trade", "Move", "Options quoted", "Price for estimates", "IV change", ""]
            .iter()
            .map(|h| format!("<th{}>{}</th>", super::tip_attr(h), h))
            .collect();
        format!(
            "<figure class=\"card wide\"><h3>Re-pricing: underlying moves since the option quotes</h3><p class=\"sub\">{} Rows with a move of {} or more are highlighted. {}</p><div class=\"tablewrap\"><table class=\"rp-table\"><thead><tr>{}</tr></thead><tbody>{}</tbody></table></div><p class=\"note\">How the estimate works: each leg's implied vol is backed out of its own quoted mid at the price the quotes reflect (one-sided or very wide quotes fall back to Alpaca's IV), then the leg is re-priced with Black-Scholes at the new price and the time left at {}, counted in trading days (weekends and market holidays don't decay). Each strike keeps its own IV (sticky strike) unless you change it. Single after-hours prints can be far from where the stock opens: check the size and time. European pricing, r = 4%, no dividends.</p></figure>",
            status,
            pyfmt_pct(rp::STALE_MOVE, 0, false),
            how,
            heads,
            rows.concat(),
            w
        )
    }

    pub fn positions(&mut self) {
        let a = self.a;
        let op = &a.open_positions;
        if op.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        let mut p: Vec<_> = op.iter().filter(|x| !x.unrealized_pl.is_nan()).collect();
        p.sort_by(|x, y| x.unrealized_pl.partial_cmp(&y.unrealized_pl).unwrap());
        let data = vec![json!({"type": "bar", "orientation": "h", "y": p.iter().map(|x| leg(&x.symbol)).collect::<Vec<_>>(),
            "x": jvec(&p.iter().map(|x| x.unrealized_pl).collect::<Vec<_>>(), 2),
            "marker": {"color": p.iter().map(|x| pn(x.unrealized_pl)).collect::<Vec<_>>()}, "showlegend": false,
            "customdata": p.iter().map(|x| json!([jr(x.market_value, 4), jr(if x.unrealized_pct.is_nan() { 0.0 } else { x.unrealized_pct }, 4)])).collect::<Vec<_>>(),
            "hovertemplate": "%{y}<br>%{x:$,.2f} (%{customdata[1]:+.1%})<br>value %{customdata[0]:$,.2f}<extra></extra>"})];
        let sub = format!(
            "Total {} on cost basis {}; cost basis is fee-inclusive.",
            money(p.iter().map(|x| x.unrealized_pl).sum(), true, 2),
            money(p.iter().map(|x| x.cost_basis).sum(), false, 2)
        );
        parts.push(self.fig("f_unreal", data, json!({"xaxis": {"tickprefix": "$"}, "margin": {"l": 150}}), (22 * p.len() as i64 + 60).max(300), "", "Unrealized P&L by position", &sub, false));
        let mut stocks: Vec<_> = op.iter().filter(|x| !x.is_option && !x.market_value.is_nan()).collect();
        stocks.sort_by(|x, y| y.market_value.partial_cmp(&x.market_value).unwrap());
        if !stocks.is_empty() {
            let tot: f64 = stocks.iter().map(|x| x.market_value).sum();
            let data = vec![json!({"type": "bar", "x": stocks.iter().map(|x| x.symbol.clone()).collect::<Vec<_>>(),
                "y": jvec(&stocks.iter().map(|x| x.market_value / tot).collect::<Vec<_>>(), 4), "marker": {"color": S[0]}, "showlegend": false,
                "customdata": jvec(&stocks.iter().map(|x| x.market_value).collect::<Vec<_>>(), 2),
                "hovertemplate": "%{x}: %{y:.1%} · %{customdata:$,.2f}<extra></extra>"})];
            let top3: f64 = stocks.iter().take(3).map(|x| x.market_value).sum();
            let sub = format!("Top 3 = {} of {}", pct(top3 / tot, false, 0), money(tot, false, 0));
            parts.push(self.fig("f_conc", data, json!({"yaxis": {"tickformat": ".0%"}, "bargap": 0.3}), 280, "", "Stock concentration (share of long stock value)", &sub, false));
        }
        let mut tb = Tbl::new(&["Symbol", "Asset", "Qty", "Avg cost (eff.)", "Broker avg", "Price", "Value", "Cost basis", "Unrealized", "%"])
            .f("Qty", super::fg())
            .f("Avg cost (eff.)", super::fnum(4))
            .f("Broker avg", super::fnum(4))
            .f("Price", super::fnum(4))
            .f("Value", super::fm(false, 2))
            .f("Cost basis", super::fm(false, 2))
            .f("Unrealized", super::fm(true, 2))
            .f("%", super::fp(true, 1));
        for x in op {
            tb.push(vec![
                Cell::s(&x.symbol),
                Cell::s(if x.is_option { "option" } else { "stock" }),
                Cell::F(x.qty),
                Cell::F(x.avg_price),
                Cell::F(x.broker_avg_price),
                Cell::F(x.price),
                Cell::F(x.market_value),
                Cell::F(x.cost_basis),
                Cell::F(x.unrealized_pl),
                Cell::F(x.unrealized_pct),
            ]);
        }
        parts.push(format!(
            "<figure class=\"card wide\"><h3>Open positions</h3><p class=\"sub\">Avg cost = fee-inclusive cost basis ÷ quantity from the ledger; “Broker avg” is Alpaca’s figure for comparison.</p>{}</figure>",
            table(&tb, "t_pos", &["Unrealized", "%"])
        ));
        self.section("positions", "Positions", parts.concat());
    }

    pub fn notes(&mut self) {
        let a = self.a;
        let mut items = Vec::new();
        match &a.reconciliation {
            Some(Recon::Matched(ok)) => items.push(format!(
                "<li><b>Ledger vs. broker positions:</b> {}</li>",
                if *ok { "rebuilt positions match the broker exactly." } else { "some rebuilt positions differ from the broker (history may be incomplete)." }
            )),
            Some(Recon::Rows(rows)) => {
                let body = if rows.is_empty() {
                    "every open quantity matches Alpaca exactly.".to_string()
                } else {
                    let mut tb = Tbl::new(&["symbol", "ledger_qty", "broker_qty", "diff"]);
                    for r in rows {
                        tb.push(vec![Cell::s(&r.symbol), Cell::F(r.ledger_qty), Cell::F(r.broker_qty), Cell::F(r.diff)]);
                    }
                    format!("{} mismatches – history may be incomplete:{}", rows.len(), table(&tb, "t_rec", &[]))
                };
                items.push(format!("<li><b>Ledger vs. broker positions:</b> {}</li>", body));
            }
            None => {}
        }
        let cs = &a.cash_summary;
        let fee_amt = if self.share { String::new() } else { format!(" {} total; {} folded into fill prices", money(cs.fees, false, 2), money(cs.fees_in_trades, false, 2)) };
        items.push(format!(
            "<li><b>Fees:</b>{} (order-linked fees to their order, daily regulatory fees to that day's fills: TAF/REG→sells, ORF/OCC→option contracts, CAT→all executions).</li>",
            if fee_amt.is_empty() { " folded into fill prices".to_string() } else { fee_amt }
        ));
        items.push("<li><b>Assignments & exercises</b> close the option at $0 and deliver stock at the strike, as Alpaca books them: the option premium stays on the option, the stock P&L starts at the strike.</li>".into());
        items.push("<li><b>Returns</b> are time-weighted (Modified Dietz, flows at start of day) so deposits never count as performance. Benchmarks are run with the same deposits on the same days.</li>".into());
        if let (Some(start), false) = (a.start, self.share) {
            let early: Vec<f64> = a.ledger.closed_df().iter().filter(|t| util::ny_date(&t.close_time) < start).map(|t| t.pnl).collect();
            if !early.is_empty() {
                items.push(format!(
                    "<li><b>Window:</b> analysis starts {}. {} earlier closed lots ({}) are excluded; set an earlier start date to include them.</li>",
                    start.format("%b %d, %Y"),
                    early.len(),
                    money(early.iter().sum(), true, 2)
                ));
            }
        }
        for w in &a.warnings {
            items.push(format!("<li>{}</li>", esc(w)));
        }
        let _: Option<HashMap<(), ()>> = None;
        self.section("notes", "Method & data checks", format!("<div class=\"card wide prose\"><ul>{}</ul></div>", items.concat()));
    }
}
