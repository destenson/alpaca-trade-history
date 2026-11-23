//! Overview, Performance and Drawdown tabs.

use serde_json::{json, Value};

use super::{isod, jf, jr, jraw, jvec, pn, profit_factor, table, Cell, Report, Tbl, GAIN, LOSS, NEUTRAL, S};
use crate::metrics;
use crate::util::{self, cummax, linspace, money, num, pct};

fn s3(i: usize) -> &'static str {
    [S[1], S[2], S[3]][i % 3]
}

impl<'a> Report<'a> {
    pub fn overview(&mut self) {
        let a = self.a;
        let t = &a.trades;
        let mut parts: Vec<String> = Vec::new();
        if !a.alerts.is_empty() {
            parts.push(format!(
                "<div class=\"alert wide\"><b>Data coverage problem</b><ul>{}</ul></div>",
                a.alerts.iter().map(|m| format!("<li>{}</li>", m)).collect::<String>()
            ));
        }
        if let Some(st) = &a.stats {
            let bs = a.bench_stats.get(&a.primary_benchmark);
            let vs = bs.map(|b| st.total_return - b.total_return).unwrap_or(f64::NAN);
            let nm = money(self.notional, false, 0);
            if self.share {
                parts.push(format!(
                    "<div class=\"card wide prose\"><b>Shared view.</b> Dollar amounts are normalized: account curves show the growth of {nm} (deposits removed); trade-level dollars are per {nm} of average account equity. Deposits, open positions, trade sizes and intraday times are not included.</div>"
                ));
            }
            let pnl: Vec<f64> = t.iter().map(|x| x.pnl).collect();
            let win = if t.is_empty() { f64::NAN } else { pnl.iter().filter(|p| **p > 0.0).count() as f64 / t.len() as f64 };
            let pb = &a.primary_benchmark;
            let first = if self.share {
                (format!("Growth of {}", nm), money(st.ending_equity, false, 0), "deposits removed".to_string())
            } else {
                ("Equity".to_string(), money(st.ending_equity, false, 0), format!("Net deposits {}", money(st.net_deposits, false, 0)))
            };
            let vs_detail = format!(
                "{} {}{}",
                pb,
                bs.map(|b| util::pct(b.total_return, true, 1)).unwrap_or_else(|| "–".into()),
                if self.share { String::new() } else { format!(" · same deposits {}", bs.map(|b| money(b.pnl, true, 0)).unwrap_or_else(|| "–".into())) }
            );
            parts.push(Self::tiles(&[
                first,
                ("Total P&L".into(), money(st.pnl, true, 0), format!("Time-weighted {}", pct(st.total_return, true, 1))),
                (format!("vs {}", pb), pct(vs, true, 1), vs_detail),
                ("Max drawdown".into(), pct(st.max_dd, false, 1), format!("{} peak-to-trough P&L", money(st.max_dd_dollar, false, 0))),
                ("Sharpe".into(), num(st.sharpe, 2), format!("Sortino {} · vol {}", num(st.sortino, 2), pct(st.vol, false, 0))),
                ("Win rate".into(), pct(win, false, 0), format!("{} closed lots · PF {}", t.len(), num(profit_factor(&pnl), 2))),
            ]));
        }
        if let Some(e) = &a.equity {
            let x: Vec<String> = e.dates.iter().map(|d| isod(*d)).collect();
            let mut data = vec![json!({"type": "scatter", "mode": "lines", "name": if self.share { "Portfolio" } else { "Account equity" }, "x": x,
                "y": jvec(&e.equity, 2), "line": {"color": S[0], "width": 2}, "hovertemplate": "%{x}<br>Equity $%{y:,.2f}<extra></extra>"})];
            if !self.share {
                data.push(json!({"type": "scatter", "mode": "lines", "name": "Net deposits", "x": x, "y": jvec(&e.net_deposits, 2),
                    "line": {"color": NEUTRAL, "width": 2, "shape": "hv", "dash": "dot"}, "hovertemplate": "%{x}<br>Deposits $%{y:,.2f}<extra></extra>"}));
            }
            for (i, (sym, bf)) in a.bench.iter().enumerate() {
                data.push(json!({"type": "scatter", "mode": "lines", "name": if self.share { sym.clone() } else { format!("{} with same deposits", sym) },
                    "x": x, "y": jvec(&bf.value, 2), "line": {"color": s3(i), "width": 2},
                    "hovertemplate": format!("%{{x}}<br>{} $%{{y:,.2f}}<extra></extra>", sym)}));
            }
            let nm = money(self.notional, false, 0);
            let title = if self.share { format!("Growth of {} vs. benchmarks", nm) } else { "Equity vs. deposits vs. benchmarks".into() };
            let sub = if self.share {
                "Time-weighted: the effect of deposits is removed."
            } else {
                "Benchmarks buy the benchmark with every deposit on the day it lands, so dollar values are directly comparable."
            };
            parts.push(self.fig("f_equity", data, json!({"yaxis": {"tickprefix": "$"}, "hovermode": "x unified"}), 360, "", &title, sub, true));
            parts.push(self.waterfall());
        }
        self.section("overview", "Overview", parts.concat());
    }

    fn waterfall(&mut self) -> String {
        let a = self.a;
        let e = a.equity.as_ref().unwrap();
        let start = e.dates[0];
        let tt: Vec<_> = a.trades.iter().filter(|t| util::ny_date(&t.close_time) >= start).collect();
        let cs = &a.cash_summary;
        let unreal_v: Vec<f64> = a.open_positions.iter().map(|p| p.unrealized_pl).filter(|v| !v.is_nan()).collect();
        let mut items: Vec<(String, f64)> = vec![
            ("Stocks realized".into(), tt.iter().filter(|t| !t.is_option).map(|t| t.pnl).sum()),
            ("Options realized".into(), tt.iter().filter(|t| t.is_option).map(|t| t.pnl).sum()),
            ("Unrealized".into(), if unreal_v.is_empty() { 0.0 } else { unreal_v.iter().sum() }),
            ("Dividends".into(), cs.income),
            ("Margin interest".into(), cs.interest),
        ];
        if self.share {
            items[2] = ("Unrealized".into(), cs.unrealized);
        }
        for it in items.iter_mut() {
            if it.1.is_nan() {
                it.1 = 0.0;
            }
        }
        let explained: f64 = items.iter().map(|x| x.1).sum();
        let st = a.stats.as_ref().unwrap();
        let (total_label, total) = if self.share {
            ("Total (per avg. equity)".to_string(), explained)
        } else {
            items.push(("Other / timing".into(), st.pnl - explained));
            ("Account P&L".to_string(), st.pnl)
        };
        let mut x: Vec<String> = items.iter().map(|x| x.0.clone()).collect();
        x.push(total_label);
        let mut y: Vec<f64> = items.iter().map(|x| util::round_dp(x.1, 2)).collect();
        y.push(util::round_dp(total, 2));
        let mut measure = vec!["relative"; items.len()];
        measure.push("total");
        let data = vec![json!({"type": "waterfall", "x": x, "y": jraw(&y), "measure": measure,
            "connector": {"line": {"color": "var:grid", "width": 1}},
            "increasing": {"marker": {"color": GAIN}}, "decreasing": {"marker": {"color": LOSS}},
            "totals": {"marker": {"color": S[0]}}, "text": y.iter().map(|v| money(*v, true, 0)).collect::<Vec<_>>(),
            "textposition": "outside", "cliponaxis": false,
            "hovertemplate": "%{x}<br>%{y:$,.2f}<extra></extra>", "showlegend": false})];
        let nm = money(self.notional, false, 0);
        let (fee_note, note) = if self.share {
            (
                format!("Per {nm} of average account equity, so the total differs from the growth-of-{nm} figure, which compounds returns over time. Fees are inside the bars."),
                String::new(),
            )
        } else {
            (
                format!("Fees ({}) are already inside the realized/unrealized bars: each fill's price is its effective price (cost ÷ quantity, fees included).", money(cs.fees, false, 2)),
                "“Other / timing” is what the ledger can’t attribute: unpriced positions (e.g. spin-offs), mark timing and rounding.".to_string(),
            )
        };
        self.fig("f_waterfall", data, json!({"yaxis": {"tickprefix": "$"}}), 340, &note, "Where the P&L came from", &format!("Since the start of the equity history. {}", fee_note), true)
    }

    pub fn performance(&mut self) {
        let a = self.a;
        let Some(e) = &a.equity else { return };
        let st = a.stats.as_ref().unwrap();
        let x: Vec<String> = e.dates.iter().map(|d| isod(*d)).collect();
        let mut parts = Vec::new();
        let minus1 = |v: &[f64]| v.iter().map(|t| t - 1.0).collect::<Vec<f64>>();
        let mut data = vec![json!({"type": "scatter", "mode": "lines", "name": "Portfolio", "x": x, "y": jvec(&minus1(&e.twr), 5), "line": {"color": S[0], "width": 2}})];
        for (i, (sym, bf)) in a.bench.iter().enumerate() {
            data.push(json!({"type": "scatter", "mode": "lines", "name": sym, "x": x, "y": jvec(&minus1(&bf.twr), 5), "line": {"color": s3(i), "width": 2}}));
        }
        parts.push(self.fig("f_twr", data, json!({"yaxis": {"tickformat": "+.0%"}, "hovermode": "x unified"}), 340, "", "Time-weighted return",
            "Deposits removed (Modified Dietz, daily) – this is the skill-only comparison against simply holding the index.", true));

        let mut data = vec![json!({"type": "scatter", "mode": "lines", "name": "Your cumulative P&L", "x": x, "y": jvec(&e.cum_pnl, 2), "line": {"color": S[0], "width": 2}})];
        for (i, (sym, bf)) in a.bench.iter().enumerate() {
            data.push(json!({"type": "scatter", "mode": "lines", "name": format!("{} buy & hold P&L", sym), "x": x, "y": jvec(&bf.cum_pnl, 2), "line": {"color": s3(i), "width": 2}}));
        }
        parts.push(self.fig("f_cumpnl", data, json!({"yaxis": {"tickprefix": "$"}, "hovermode": "x unified"}), 320, "", "Cumulative P&L in dollars vs. buy & hold", "", false));

        let data = vec![json!({"type": "bar", "x": x, "y": jvec(&e.pnl, 2), "name": "Daily P&L",
            "marker": {"color": e.pnl.iter().map(|v| pn(*v)).collect::<Vec<_>>()},
            "hovertemplate": "%{x}<br>%{y:$,.2f}<extra></extra>", "showlegend": false})];
        let sub = format!("{} of days were up · best {} · worst {}", pct(st.pct_up_days, false, 0), pct(st.best_day, true, 1), pct(st.worst_day, true, 1));
        parts.push(self.fig("f_daily", data, json!({"yaxis": {"tickprefix": "$"}, "bargap": 0.15}), 280, "", "Daily P&L", &sub, false));

        let mo = &a.monthly;
        if !mo.is_empty() {
            let mx: Vec<String> = mo.iter().map(|m| util::fmt_d(util::date_ymd(m.year, m.month, 1), "%b %Y")).collect();
            let mut data = vec![json!({"type": "bar", "name": "Portfolio", "x": mx, "y": Value::Array(mo.iter().map(|m| jr(m.ret, 5)).collect()),
                "marker": {"color": S[0]}, "hovertemplate": "%{x}<br>%{y:+.1%}<extra>Portfolio</extra>"})];
            if let Some(pb) = a.bench.get(&a.primary_benchmark) {
                let br = metrics::monthly(&e.dates, &pb.ret, None);
                data.push(json!({"type": "bar", "name": a.primary_benchmark, "x": mx, "y": Value::Array(br.iter().map(|m| jr(m.ret, 5)).collect()),
                    "marker": {"color": S[1]}, "hovertemplate": format!("%{{x}}<br>%{{y:+.1%}}<extra>{}</extra>", a.primary_benchmark)}));
            }
            parts.push(self.fig("f_monthly", data, json!({"yaxis": {"tickformat": "+.0%"}, "barmode": "group", "bargap": 0.3, "bargroupgap": 0.08}), 280, "", "Monthly returns", "", false));
        }

        if let Some(pb) = a.bench.get(&a.primary_benchmark) {
            let r = &e.ret[1..];
            let b: Vec<f64> = pb.ret[1..].iter().map(|v| if v.is_nan() { 0.0 } else { *v }).collect();
            let xs = linspace(util::vmin(&b), util::vmax(&b), 20);
            let p = &a.primary_benchmark;
            let data = vec![
                json!({"type": "scatter", "mode": "markers", "name": "Days", "x": jvec(&b, 5), "y": jvec(r, 5),
                    "text": e.dates[1..].iter().map(|d| isod(*d)).collect::<Vec<_>>(),
                    "marker": {"color": S[0], "size": 8, "opacity": 0.75, "line": {"color": "var:surface", "width": 1}},
                    "hovertemplate": format!("%{{text}}<br>{} %{{x:+.2%}}<br>You %{{y:+.2%}}<extra></extra>", p)}),
                json!({"type": "scatter", "mode": "lines", "name": format!("β = {}", num(st.beta, 2)), "x": jraw(&xs),
                    "y": jraw(&xs.iter().map(|x| st.alpha / 252.0 + st.beta * x).collect::<Vec<_>>()), "line": {"color": S[1], "width": 2}}),
            ];
            let sub = format!(
                "β {} · correlation {} · annualized α {} · up-capture {} · down-capture {}",
                num(st.beta, 2), num(st.correlation, 2), pct(st.alpha, true, 1), pct(st.up_capture, false, 0), pct(st.down_capture, false, 0)
            );
            parts.push(self.fig("f_beta", data, json!({"xaxis": {"tickformat": "+.1%", "title": {"text": format!("{} daily return", p)}},
                "yaxis": {"tickformat": "+.0%", "title": {"text": "Your daily return"}}}), 340, "", &format!("Daily returns vs. {}", p), &sub, false));
        }

        if let Some(ro) = &a.rolling {
            if ro.vol.iter().filter(|v| !v.is_nan()).count() > 2 {
                let data = vec![json!({"type": "scatter", "mode": "lines", "name": "Rolling vol (21d, ann.)", "x": x, "y": jvec(&ro.vol, 4), "line": {"color": S[0], "width": 2}, "showlegend": false})];
                parts.push(self.fig("f_rvol", data, json!({"yaxis": {"tickformat": ".0%"}}), 240, "", "Rolling 21-day volatility (annualized)", "", false));
                let data = vec![json!({"type": "scatter", "mode": "lines", "name": "Rolling Sharpe", "x": x, "y": jvec(&ro.sharpe, 3), "line": {"color": S[0], "width": 2}, "showlegend": false})];
                parts.push(self.fig("f_rsharpe", data, json!({}), 240, "", "Rolling 21-day Sharpe", "", false));
                if let Some(beta) = &ro.beta {
                    let data = vec![json!({"type": "scatter", "mode": "lines", "name": "Rolling beta", "x": x, "y": jvec(beta, 3), "line": {"color": S[0], "width": 2}, "showlegend": false})];
                    let t = format!("Rolling 21-day beta to {}", a.primary_benchmark);
                    parts.push(self.fig("f_rbeta", data, json!({}), 240, "", &t, "", false));
                }
            }
        }

        let pnl_col = if self.share { format!("P&L on {}", money(self.notional, false, 0)) } else { "P&L on your deposits".to_string() };
        let mut tb = Tbl::new(&["", "Total return", "Annualized", "Volatility", "Sharpe", "Sortino", "Max drawdown", "Calmar"]);
        tb.cols.push(pnl_col);
        let mut all = vec![("Portfolio".to_string(), st)];
        for (k, v) in &a.bench_stats {
            all.push((k.clone(), v));
        }
        for (name, s) in all {
            tb.push(vec![
                Cell::s(name),
                Cell::s(pct(s.total_return, true, 1)),
                Cell::s(pct(s.cagr, true, 1)),
                Cell::s(pct(s.vol, false, 1)),
                Cell::s(num(s.sharpe, 2)),
                Cell::s(num(s.sortino, 2)),
                Cell::s(pct(s.max_dd, false, 1)),
                Cell::s(num(s.calmar, 2)),
                Cell::s(money(s.pnl, true, 2)),
            ]);
        }
        parts.push(format!(
            "<figure class=\"card wide\"><h3>Risk & return summary</h3><p class=\"sub\">Benchmarks are run with your exact deposit dates and amounts. Short histories make annualized numbers noisy – read them as direction, not forecast.</p>{}</figure>",
            table(&tb, "t_riskret", &[])
        ));
        self.section("performance", "Performance", parts.concat());
    }

    pub fn drawdown(&mut self) {
        let a = self.a;
        let Some(e) = &a.equity else { return };
        let x: Vec<String> = e.dates.iter().map(|d| isod(*d)).collect();
        let mut data = vec![json!({"type": "scatter", "mode": "lines", "name": "Portfolio", "x": x, "y": jvec(&e.dd, 5), "fill": "tozeroy", "line": {"color": LOSS, "width": 2}, "fillcolor": "var:loss-fill"})];
        for (i, (sym, bf)) in a.bench.iter().enumerate() {
            data.push(json!({"type": "scatter", "mode": "lines", "name": sym, "x": x, "y": jvec(&bf.dd, 5), "line": {"color": s3(i), "width": 2}}));
        }
        let dd_now = *e.dd.last().unwrap();
        let argmax = |v: &[f64]| {
            let mut k = 0;
            for i in 0..v.len() {
                if v[i] > v[k] {
                    k = i;
                }
            }
            k
        };
        let peak_d = e.dates[argmax(&e.twr)];
        let deepest = util::vmin(&e.dd);
        let now_txt = if dd_now < -1e-9 {
            format!("Now {} below the {} {} peak", pct(dd_now, false, 1), peak_d.format("%b"), chrono::Datelike::day(&peak_d))
        } else {
            "At a new high".to_string()
        };
        let sub = format!(
            "How far the deposit-neutral return index is below its own best level so far — a fall from the peak, not a loss on your deposits. {}; deepest {}.",
            now_txt,
            pct(deepest, false, 1)
        );
        let mut parts = vec![self.fig("f_underwater", data, json!({"yaxis": {"tickformat": ".0%"}, "hovermode": "x unified"}), 320, "", "Underwater curve (time-weighted)", &sub, true)];
        let cum = &e.cum_pnl;
        let hwm = cummax(cum);
        let gap: Vec<f64> = cum.iter().zip(&hwm).map(|(c, h)| c - h).collect();
        let data = vec![
            json!({"type": "scatter", "mode": "lines", "name": "Peak so far (high-water mark)", "x": x, "y": jvec(&hwm, 2),
                "line": {"color": NEUTRAL, "width": 1.5, "dash": "dot", "shape": "hv"}, "hovertemplate": "peak %{y:$,.2f}<extra></extra>"}),
            json!({"type": "scatter", "mode": "lines", "name": "Cumulative P&L", "x": x, "y": jvec(cum, 2),
                "fill": "tonexty", "fillcolor": "var:loss-fill", "line": {"color": S[0], "width": 2}, "customdata": jvec(&gap, 2),
                "hovertemplate": "P&L %{y:$,.2f}<br>%{customdata:$,.2f} vs. the peak<extra></extra>"}),
        ];
        let (now, top) = (*cum.last().unwrap(), *hwm.last().unwrap());
        let top_d = e.dates[argmax(cum)];
        let sub = format!(
            "Now {} on your deposits{}. The shaded gap is the drawdown: the distance from the best level reached so far.",
            money(now, true, 0),
            if top - now > 0.5 {
                format!(", {} below the peak of {} on {} {}", money(top - now, false, 0), money(top, true, 0), top_d.format("%b"), chrono::Datelike::day(&top_d))
            } else {
                ", at its peak".to_string()
            }
        );
        parts.push(self.fig("f_dd_dollar", data, json!({"yaxis": {"tickprefix": "$"}, "hovermode": "x unified",
            "shapes": [{"type": "line", "xref": "paper", "x0": 0, "x1": 1, "y0": 0, "y1": 0, "line": {"color": "var:axis", "width": 1}}]}),
            300, "", "Cumulative P&L and its high-water mark", &sub, false));
        if !a.drawdowns.is_empty() {
            let mut tb = Tbl::new(&["Peak", "Trough", "Recovered", "Depth", "Days"]).f("Depth", super::fp(false, 1));
            for d in &a.drawdowns {
                tb.push(vec![
                    Cell::s(isod(d.peak)),
                    Cell::s(isod(d.trough)),
                    Cell::s(d.recovered.map(isod).unwrap_or_else(|| "not yet".into())),
                    Cell::F(d.depth),
                    Cell::I(d.length_days),
                ]);
            }
            parts.push(format!("<figure class=\"card\"><h3>Worst drawdowns</h3>{}</figure>", table(&tb, "t_dd", &[])));
        }
        let _ = jf;
        self.section("drawdown", "Drawdown", parts.concat());
    }
}
