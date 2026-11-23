//! Trades tab and the shared "total + count + median" distribution chart.

use std::collections::BTreeMap;

use chrono::{Datelike, Timelike};
use serde_json::{json, Value};

use super::{dur, iso, jf, jr, jvec, obj, pn, profit_factor, table, Cell, Report, Tbl, GAIN, LOSS, S};
use crate::ledger::Trade;
use crate::util::{self, money, num, pct, round_dp};

pub struct Group {
    pub lab: String,
    pub tk: String,
    pub pnl: Vec<f64>,
}

fn streaks(pnl: &[f64]) -> (usize, usize) {
    let (mut bw, mut bl, mut cw, mut cl) = (0, 0, 0, 0);
    for &v in pnl {
        if v > 0.0 {
            cw += 1;
            cl = 0;
        } else if v < 0.0 {
            cl += 1;
            cw = 0;
        }
        bw = bw.max(cw);
        bl = bl.max(cl);
    }
    (bw, bl)
}

fn nice_bins(v: &[f64], target: f64) -> Vec<f64> {
    let lo = util::vmin(v);
    let hi = util::vmax(v);
    let span = (hi - lo).max(1.0);
    let raw = span / target;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(raw);
    let start = (lo / step).floor() * step;
    let stop = hi + step;
    let n = ((stop - start) / step).ceil().max(0.0) as usize;
    let edges: Vec<f64> = (0..n).map(|i| start + i as f64 * step).collect();
    if edges.len() > 1 {
        edges
    } else {
        vec![lo - 1.0, hi + 1.0]
    }
}

fn histogram(v: &[f64], edges: &[f64]) -> Vec<i64> {
    let nb = edges.len() - 1;
    let mut c = vec![0i64; nb];
    for &x in v {
        if x < edges[0] || x > edges[nb] || x.is_nan() {
            continue;
        }
        let mut i = edges.partition_point(|e| *e <= x);
        i = i.saturating_sub(1);
        if i >= nb {
            i = nb - 1;
        }
        c[i] += 1;
    }
    c
}

impl<'a> Report<'a> {
    pub fn trades(&mut self) {
        let a = self.a;
        if a.trades.is_empty() {
            return;
        }
        let mut t: Vec<&Trade> = a.trades.iter().collect();
        t.sort_by(|x, y| x.close_time.cmp(&y.close_time));
        let pnl: Vec<f64> = t.iter().map(|x| x.pnl).collect();
        let w: Vec<f64> = pnl.iter().copied().filter(|p| *p > 0.0).collect();
        let l: Vec<f64> = pnl.iter().copied().filter(|p| *p < 0.0).collect();
        let (sw, sl) = streaks(&pnl);
        let n = t.len();
        let fees: f64 = t.iter().map(|x| x.fees).sum();
        let (wm, lm) = (util::mean(&w), util::mean(&l));
        let mut parts = vec![Self::tiles(&[
            ("Realized P&L".into(), money(pnl.iter().sum(), true, 0), format!("{} closed lots{}", n, if self.share { String::new() } else { format!(" · fees {}", money(fees, false, 2)) })),
            ("Win rate".into(), pct(w.len() as f64 / n as f64, false, 0), format!("{} wins / {} losses", w.len(), l.len())),
            ("Avg win / loss".into(), format!("{} / {}", money(wm, false, 0), money(lm, false, 0)),
                format!("payoff ratio {}", num(if !l.is_empty() && !w.is_empty() { (wm / lm).abs() } else { f64::NAN }, 2))),
            ("Profit factor".into(), num(profit_factor(&pnl), 2), format!("expectancy {} per lot", money(util::mean(&pnl), true, 2))),
            ("Largest win / loss".into(), format!("{} / {}", money(util::vmax(&pnl), false, 0), money(util::vmin(&pnl), false, 0)), format!("streaks: {} wins · {} losses", sw, sl)),
            ("Median hold".into(), dur(util::median(&t.iter().map(|x| x.hold_days).collect::<Vec<_>>())), format!("mean {}", dur(util::mean(&t.iter().map(|x| x.hold_days).collect::<Vec<_>>())))),
        ])];

        let mut data = Vec::new();
        for (is_opt, col, name) in [(false, S[0], "Stocks"), (true, S[1], "Options")] {
            let g: Vec<&&Trade> = t.iter().filter(|x| x.is_option == is_opt).collect();
            if !g.is_empty() {
                data.push(json!({"type": "scatter", "mode": "lines", "name": name, "x": g.iter().map(|x| iso(&x.close_time)).collect::<Vec<_>>(),
                    "y": jvec(&util::cumsum(&g.iter().map(|x| x.pnl).collect::<Vec<_>>()), 2), "line": {"color": col, "width": 2, "shape": "hv"}}));
            }
        }
        data.push(json!({"type": "scatter", "mode": "lines", "name": "All", "x": t.iter().map(|x| iso(&x.close_time)).collect::<Vec<_>>(),
            "y": jvec(&util::cumsum(&pnl), 2), "line": {"color": S[2], "width": 2, "shape": "hv"}}));
        parts.push(self.fig("f_cumreal", data, json!({"yaxis": {"tickprefix": "$"}}), 320, "", "Cumulative realized P&L", "Stepped at each close; FIFO lot matching, fees included.", true));

        let edges = nice_bins(&pnl, 24.0);
        let counts = histogram(&pnl, &edges);
        let centers: Vec<f64> = (0..counts.len()).map(|i| (edges[i] + edges[i + 1]) / 2.0).collect();
        let data = vec![json!({"type": "bar", "x": jvec(&centers, 2), "y": counts, "width": super::jraw(&(0..counts.len()).map(|i| (edges[i + 1] - edges[i]) * 0.9).collect::<Vec<_>>()),
            "marker": {"color": centers.iter().map(|c| pn(*c)).collect::<Vec<_>>()}, "showlegend": false,
            "customdata": (0..counts.len()).map(|i| json!([jr(edges[i], 0), jr(edges[i + 1], 0)])).collect::<Vec<_>>(),
            "hovertemplate": "$%{customdata[0]:,} to $%{customdata[1]:,}<br>%{y} lots<extra></extra>"})];
        parts.push(self.fig("f_hist", data, json!({"xaxis": {"tickprefix": "$"}, "yaxis": {"title": {"text": "Lots"}}}), 280, "", "P&L distribution per closed lot", "", false));

        let (yfmt, hfmt) = if self.share { (json!({"tickformat": "+.0%"}), "%{y:+.1%}") } else { (json!({"tickprefix": "$"}), "%{y:$,.2f}") };
        let mut data = Vec::new();
        for (is_opt, col, name) in [(false, S[0], "Stocks"), (true, S[1], "Options")] {
            let g: Vec<&&Trade> = t.iter().filter(|x| x.is_option == is_opt).collect();
            if !g.is_empty() {
                let ys: Vec<f64> = g.iter().map(|x| if self.share { x.ret } else { x.pnl }).collect();
                data.push(json!({"type": "scatter", "mode": "markers", "name": name,
                    "x": jvec(&g.iter().map(|x| x.hold_days.max(1.0 / 1440.0)).collect::<Vec<_>>(), 4), "y": jvec(&ys, 4),
                    "text": g.iter().map(|x| x.symbol.clone()).collect::<Vec<_>>(),
                    "marker": {"color": col, "size": 9, "opacity": 0.8, "line": {"color": "var:surface", "width": 1}},
                    "hovertemplate": format!("%{{text}}<br>held %{{x:.2f}} d<br>{}<extra></extra>", hfmt)}));
            }
        }
        let title = format!("Holding period vs. {}", if self.share { "return" } else { "P&L" });
        parts.push(self.fig("f_hold", data, json!({"xaxis": {"type": "log", "title": {"text": "Days held (log)"}}, "yaxis": yfmt}), 300, "", &title, "", false));

        let sy = &a.symbols;
        if !sy.is_empty() {
            let mut s2: Vec<_> = sy.iter().filter(|r| r.trades > 0.0).collect();
            s2.sort_by(|x, y| x.realized.partial_cmp(&y.realized).unwrap_or(std::cmp::Ordering::Equal));
            let data = vec![json!({"type": "bar", "orientation": "h", "y": s2.iter().map(|r| r.underlying.clone()).collect::<Vec<_>>(),
                "x": jvec(&s2.iter().map(|r| r.realized).collect::<Vec<_>>(), 2),
                "marker": {"color": s2.iter().map(|r| pn(r.realized)).collect::<Vec<_>>()}, "showlegend": false,
                "customdata": s2.iter().map(|r| json!([jr(r.stock_pnl, 2), jr(r.option_pnl, 2), jr(r.trades, 2)])).collect::<Vec<_>>(),
                "hovertemplate": "<b>%{y}</b><br>Realized %{x:$,.2f}<br>Stock %{customdata[0]:$,.2f}<br>Options %{customdata[1]:$,.2f}<br>%{customdata[2]} closed lots<extra></extra>"})];
            let n_open = s2.iter().filter(|r| r.is_open).count();
            let sub = format!(
                "Closed lots only (stock + options, fees included). Open positions and dividends are excluded{}.",
                if self.share || n_open == 0 { String::new() } else { format!("; {} of these names still have open positions, shown on the Options and Positions tabs", n_open) }
            );
            parts.push(self.fig("f_bysym", data, json!({"xaxis": {"tickprefix": "$"}, "margin": {"l": 84}}), (22 * s2.len() as i64 + 60).max(300), "", "Realized P&L by underlying", &sub, false));
            let mut s3: Vec<_> = s2.iter().filter(|r| r.bhc_pnl.is_some()).collect();
            if !s3.is_empty() {
                s3.sort_by(|x, y| x.edge_closed.unwrap().partial_cmp(&y.edge_closed.unwrap()).unwrap_or(std::cmp::Ordering::Equal));
                let ys: Vec<String> = s3.iter().map(|r| r.underlying.clone()).collect();
                let data = vec![
                    json!({"type": "bar", "orientation": "h", "name": "Your realized P&L", "y": ys, "x": jvec(&s3.iter().map(|r| r.realized).collect::<Vec<_>>(), 2), "marker": {"color": S[0]}}),
                    json!({"type": "bar", "orientation": "h", "name": "Buy & hold P&L", "y": ys, "x": jvec(&s3.iter().map(|r| r.bhc_pnl.unwrap()).collect::<Vec<_>>(), 2), "marker": {"color": S[1]},
                        "customdata": s3.iter().map(|r| json!([jf(r.bhc_capital.unwrap()), jf(r.bhc_return.unwrap())])).collect::<Vec<_>>(),
                        "text": s3.iter().map(|r| super::isod(r.bhc_end.unwrap())).collect::<Vec<_>>(),
                        "hovertemplate": "%{y}: %{x:$,.2f}<br>%{customdata[0]:$,.0f} held from first trade to your last close %{text} (%{customdata[1]:+.1%})<extra>Buy & hold</extra>"}),
                ];
                parts.push(self.fig("f_edge", data, json!({"barmode": "group", "xaxis": {"tickprefix": "$"}, "margin": {"l": 84}, "bargap": 0.25}),
                    (30 * s3.len() as i64 + 80).max(300), "", "Your trading vs. just holding the underlying",
                    "Realized P&L of closed lots vs. buy & hold over the same span: your peak capital committed to that name up to your last closed lot, bought on your first trade date and sold on that last close. Positions still open are not counted. Sorted by your edge.", false));
            }
        }

        // timing
        let wd: Vec<u32> = t.iter().map(|x| util::to_ny(&x.open_time).weekday().num_days_from_monday()).collect();
        let hr: Vec<u32> = t.iter().map(|x| util::to_ny(&x.open_time).hour()).collect();
        let mut days: Vec<u32> = (0..5).collect();
        for d in &wd {
            if !days.contains(d) {
                days.push(*d);
            }
        }
        days.sort();
        let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let groups: Vec<Group> = days
            .iter()
            .map(|d| Group { lab: names[*d as usize].into(), tk: names[*d as usize].into(), pnl: (0..n).filter(|&i| wd[i] == *d).map(|i| pnl[i]).collect() })
            .collect();
        parts.push(self.dist_fig("f_wd", groups, "Realized P&L by entry weekday", "", false, "lots", "", "number of closed lots under each label", None));
        if !self.share {
            let mut hrs: Vec<u32> = hr.clone();
            hrs.sort();
            hrs.dedup();
            let groups: Vec<Group> = hrs.iter().map(|h| Group { lab: format!("{:02}:00", h), tk: h.to_string(), pnl: (0..n).filter(|&i| hr[i] == *h).map(|i| pnl[i]).collect() }).collect();
            parts.push(self.dist_fig("f_hr", groups, "Realized P&L by entry hour (ET)", "Includes overnight-session entries. ", false, "lots", " ET",
                "number of closed lots under each label · hour of entry, ET", None));
        }

        // gantt
        let mut first: BTreeMap<&str, &util::Ts> = BTreeMap::new();
        for x in &t {
            let e = first.entry(x.underlying.as_str()).or_insert(&x.open_time);
            if x.open_time < **e {
                *e = &x.open_time;
            }
        }
        let mut order: Vec<(&str, &util::Ts)> = first.into_iter().collect();
        order.sort_by(|x, y| y.1.cmp(x.1));
        let order: Vec<&str> = order.into_iter().map(|x| x.0).collect();
        let mut data = Vec::new();
        for (win, col, name) in [(true, GAIN, "Winning lot"), (false, LOSS, "Losing lot")] {
            let (mut xs, mut ys, mut tx) = (Vec::new(), Vec::new(), Vec::new());
            for r in t.iter().filter(|x| if win { x.pnl >= 0.0 } else { x.pnl < 0.0 }) {
                xs.extend([json!(iso(&r.open_time)), json!(iso(&r.close_time)), Value::Null]);
                ys.extend([json!(r.underlying), json!(r.underlying), Value::Null]);
                let size = if self.share { String::new() } else { format!(" ×{}", util::g(r.qty)) };
                let outcome = if self.share { pct(r.ret, true, 1) } else { money(r.pnl, true, 2) };
                let lab = format!("{} {}{}: {}", r.symbol, r.direction(), size, outcome);
                tx.extend([json!(lab), json!(lab), Value::Null]);
            }
            data.push(json!({"type": "scatter", "mode": "lines+markers", "name": name, "x": xs, "y": ys, "text": tx,
                "line": {"color": col, "width": 5}, "marker": {"size": 8, "color": col}, "hovertemplate": "%{text}<extra></extra>", "connectgaps": false}));
        }
        parts.push(self.fig("f_gantt", data, json!({"yaxis": {"categoryorder": "array", "categoryarray": order}, "margin": {"l": 84}}),
            (20 * order.len() as i64 + 80).max(320), "", "Trade timeline", "Each bar is one closed lot from entry to exit.", true));

        let mut tb = Tbl::new(&["Closed", "Symbol", "Asset", "Side", "Qty", "Entry (eff.)", "Exit (eff.)", "How closed", "Days", "Fees", "P&L", "Return"])
            .f("Qty", |c| util::g(c.f()))
            .f("Entry (eff.)", super::fnum(4))
            .f("Exit (eff.)", super::fnum(4))
            .f("Days", super::fnum(1))
            .f("Fees", super::fm(false, 2))
            .f("P&L", super::fm(true, 2))
            .f("Return", super::fp(true, 1));
        for r in desc_by_close(&t) {
            tb.push(vec![
                Cell::s(iso(&r.close_time)),
                Cell::s(&r.symbol),
                Cell::s(r.asset()),
                Cell::s(r.direction()),
                Cell::F(r.qty),
                Cell::F(r.open_price),
                Cell::F(r.close_price),
                Cell::s(r.close_kind),
                Cell::F(r.hold_days),
                Cell::F(r.fees),
                Cell::F(r.pnl),
                Cell::F(r.ret),
            ]);
        }
        if self.share {
            tb.drop_cols(&["Qty", "Entry (eff.)", "Exit (eff.)", "Fees", "P&L"]);
        }
        let sub = if self.share {
            "Click a header to sort. Return is on the lot's fee-inclusive cost; per-lot dollar amounts are omitted in the shared view."
        } else {
            "Click a header to sort. Entry/exit are effective prices: net cash ÷ (qty × multiplier), fees included."
        };
        parts.push(format!("<figure class=\"card wide\"><h3>All closed lots</h3><p class=\"sub\">{}</p>{}</figure>", sub, table(&tb, "t_trades", &["P&L", "Return"])));
        self.section("trades", "Trades", parts.concat());
    }

    /// Total P&L per group (bars, with the group's size in its label) and the median item (diamonds on a
    /// second axis whose zero lines up with the bars').
    #[allow(clippy::too_many_arguments)]
    pub fn dist_fig(&mut self, fid: &str, groups: Vec<Group>, title: &str, sub: &str, horizontal: bool, unit: &str, hover_suffix: &str, axis_title: &str, height: Option<i64>) -> String {
        struct Row {
            lab: String,
            tk: String,
            n: usize,
            sum: f64,
            median: Option<f64>,
            win: Option<f64>,
            best: Option<f64>,
            worst: Option<f64>,
            ex_worst: Option<f64>,
        }
        let rows: Vec<Row> = groups
            .into_iter()
            .map(|g| {
                let p: Vec<f64> = g.pnl.into_iter().filter(|x| !x.is_nan()).collect();
                let n = p.len();
                let s: f64 = p.iter().sum();
                let mn = util::vmin(&p);
                Row {
                    lab: g.lab,
                    tk: g.tk,
                    n,
                    sum: if n > 0 { s } else { 0.0 },
                    median: if n > 0 { Some(util::median(&p)) } else { None },
                    win: if n > 0 { Some(p.iter().filter(|x| **x > 0.0).count() as f64 / n as f64) } else { None },
                    best: if n > 0 { Some(util::vmax(&p)) } else { None },
                    worst: if n > 0 { Some(mn) } else { None },
                    ex_worst: if n > 1 && mn < 0.0 { Some(s - mn) } else { None },
                }
            })
            .collect();
        let fmt = |v: Option<f64>| v.filter(|x| !x.is_nan()).map(|x| money(x, true, 2)).unwrap_or_else(|| "–".into());
        let labels: Vec<String> = rows.iter().map(|r| r.lab.clone()).collect();
        let sums: Vec<f64> = rows.iter().map(|r| round_dp(r.sum, 2)).collect();
        let meds: Vec<Option<f64>> = rows.iter().map(|r| r.median.map(|m| round_dp(m, 2))).collect();
        let cd: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!([r.n, r.win.map(|w| util::pyfmt_pct(w, 0, false)).unwrap_or_else(|| "–".into()), fmt(r.median), fmt(r.best), fmt(r.worst),
                    if r.ex_worst.is_some() { fmt(r.ex_worst) } else { "n/a".into() }, format!("{}{}", r.lab, hover_suffix)])
            })
            .collect();
        let v = if horizontal { "x" } else { "y" };
        let ht = format!("<b>%{{customdata[6]}}</b>: total %{{{v}:$,.2f}} over %{{customdata[0]}} {unit}<br>win rate %{{customdata[1]}} · median %{{customdata[2]}}<br>best %{{customdata[3]}} · worst %{{customdata[4]}}<br>total without the worst one %{{customdata[5]}}<extra></extra>");
        let mut bar = obj(json!({"type": "bar", "name": "Total", "customdata": cd, "marker": {"color": sums.iter().map(|x| pn(*x)).collect::<Vec<_>>()}, "hovertemplate": ht}));
        let one = if let Some(s) = unit.strip_suffix('s') { s } else { unit };
        let mut dia = obj(json!({"type": "scatter", "mode": "markers", "name": format!("Median {}", one),
            "marker": {"color": "var:ink", "symbol": "diamond", "size": 8, "line": {"width": 1, "color": "var:surface"}}, "customdata": cd,
            "hovertemplate": format!("<b>%{{customdata[6]}}</b>: median %{{{v}:$,.2f}} (%{{customdata[0]}} {unit})<extra></extra>")}));
        let medj: Vec<Value> = meds.iter().map(|m| m.map(jf).unwrap_or(Value::Null)).collect();
        if horizontal {
            bar.insert("orientation".into(), json!("h"));
            bar.insert("y".into(), json!(labels));
            bar.insert("x".into(), super::jraw(&sums));
            dia.insert("y".into(), json!(labels));
            dia.insert("x".into(), json!(medj));
            dia.insert("xaxis".into(), json!("x2"));
        } else {
            bar.insert("x".into(), json!(labels));
            bar.insert("y".into(), super::jraw(&sums));
            dia.insert("x".into(), json!(labels));
            dia.insert("y".into(), json!(medj));
            dia.insert("yaxis".into(), json!("y2"));
        }
        let mut lo = sums.iter().cloned().fold(0.0f64, f64::min);
        let mut hi = sums.iter().cloned().fold(0.0f64, f64::max);
        let span = if hi - lo != 0.0 { hi - lo } else { 1.0 };
        lo = (lo - 0.06 * span).min(-0.08 * span);
        hi = (hi + 0.06 * span).max(0.08 * span);
        let mv: Vec<f64> = meds.iter().flatten().copied().collect();
        let mut ks: Vec<f64> = mv.iter().filter(|m| **m > 0.0).map(|m| m / hi).collect();
        ks.extend(mv.iter().filter(|m| **m < 0.0).map(|m| m / lo));
        ks.push(0.0);
        let k0 = ks.iter().cloned().fold(f64::NEG_INFINITY, f64::max) * 1.1;
        let k = if k0 != 0.0 { k0 } else { 1.0 };
        let mut main = obj(json!({"tickprefix": "$", "range": [lo, hi], "title": {"text": "total"}}));
        let second = obj(json!({"overlaying": v, "tickprefix": "$", "range": [lo * k, hi * k], "showgrid": false, "zeroline": false, "title": {"text": "median"}}));
        let mut layout;
        let h;
        if horizontal {
            let ticktext: Vec<String> = rows.iter().map(|r| format!("{} <span style='font-size:10px'>· {}</span>", r.tk, r.n)).collect();
            let cat = json!({"tickmode": "array", "tickvals": labels, "ticktext": ticktext});
            main.insert("title".into(), json!({"text": ""}));
            main.insert("ticksuffix".into(), json!(""));
            let mut x2 = second.clone();
            x2.insert("side".into(), json!("top"));
            x2.insert("title".into(), json!({"text": ""}));
            x2.insert("ticksuffix".into(), json!(" median"));
            layout = obj(json!({"xaxis": Value::Object(main), "xaxis2": Value::Object(x2), "yaxis": cat, "margin": {"l": 180, "t": 40}}));
            h = height.unwrap_or((30 * rows.len() as i64 + 110).max(280));
            layout.insert("showlegend".into(), json!(true));
            layout.insert("legend".into(), json!({"orientation": "h", "y": -0.06, "yanchor": "top", "x": 1, "xanchor": "right"}));
        } else {
            let ticktext: Vec<String> = rows.iter().map(|r| format!("{}<br><span style='font-size:10px'>{}</span>", r.tk, r.n)).collect();
            let at = if axis_title.is_empty() { format!("number of {} under each label", unit) } else { axis_title.to_string() };
            let cat = json!({"tickmode": "array", "tickvals": labels, "ticktext": ticktext, "tickangle": 0, "title": {"text": at}});
            let mut y2 = second.clone();
            y2.insert("side".into(), json!("right"));
            layout = obj(json!({"xaxis": cat, "yaxis": Value::Object(main), "yaxis2": Value::Object(y2), "margin": {"r": 56}, "bargap": if rows.len() > 8 { 0.3 } else { 0.35 }}));
            h = height.unwrap_or(380);
            layout.insert("showlegend".into(), json!(true));
            layout.insert("legend".into(), json!({"orientation": "h", "y": 1.02, "yanchor": "bottom", "x": 1, "xanchor": "right"}));
        }
        let subtitle = format!(
            "{}Bars: total, with the number of {} by each label. Diamonds: the median (second axis); when it disagrees with the bar, a few big results decide the total.",
            if sub.is_empty() { String::new() } else { format!("{} ", sub) },
            unit
        );
        self.fig(fid, vec![Value::Object(bar), Value::Object(dia)], Value::Object(layout), h, "", title, &subtitle, false)
    }
}

/// Newest first; ties keep their original order (pandas' descending sort).
fn desc_by_close<'b>(t: &[&'b Trade]) -> Vec<&'b Trade> {
    let mut v: Vec<&Trade> = t.to_vec();
    v.sort_by(|x, y| y.close_time.cmp(&x.close_time));
    v
}
