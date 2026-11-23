"""Render an :class:`~alpaca_analyzer.analysis.Analysis` to one self-contained HTML file.

Charts are emitted as Plotly JSON specs. Colors are written as ``var:<token>`` and resolved
in the browser from CSS custom properties, so the whole report follows light/dark mode.
"""
from __future__ import annotations

import html
import json
import math
from datetime import date, datetime
from pathlib import Path

import numpy as np
import pandas as pd

from .analysis import Analysis
from .ledger import NY
from .occ import parse_occ
from .interpret import build_units, readings
from . import orders as O
from . import reprice as RP
from .reinvest import reinvest_table
from .glossary import tip as _tip
from .optionsmath import payoff_curves

PLOTLY_CDN = "https://cdn.jsdelivr.net/npm/plotly.js-dist-min@2.35.2/plotly.min.js"
PLOTLY_SRI = "sha384-cCVCZkAjYNxaYKbM8lsArLznDF/SvMFr1jcZrvOpSTCa0W40ZAdLzHCEulnUa5i7"
TEMPLATE = Path(__file__).with_name("templates") / "report.html"

S1, S2, S3, S4, S5, S6, S7, S8 = (f"var:series-{i}" for i in range(1, 9))
GAIN, LOSS, NEUTRAL = "var:gain", "var:loss", "var:muted"


# ------------------------------------------------------------------ formatting
def _num(x):
    return None if x is None or (isinstance(x, float) and (math.isnan(x) or math.isinf(x))) else x


def money(x, signed=False, dp=2):
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "–"
    s = f"{abs(x):,.{dp}f}"
    sign = "−" if x < 0 else ("+" if signed and x > 0 else "")
    return f"{sign}${s}"


def pct(x, signed=False, dp=1):
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "–"
    sign = "−" if x < 0 else ("+" if signed and x > 0 else "")
    return f"{sign}{abs(x) * 100:,.{dp}f}%"


def num(x, dp=2):
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "–"
    return f"{x:,.{dp}f}".replace("-", "−")


def _iso(d):
    if isinstance(d, (pd.Timestamp, datetime)):
        ts = pd.Timestamp(d)
        if ts.tzinfo:
            ts = ts.tz_convert(NY)
        return ts.strftime("%Y-%m-%d") if (ts.hour, ts.minute, ts.second) == (0, 0, 0) else ts.strftime("%Y-%m-%d %H:%M")
    return d.isoformat() if isinstance(d, date) else str(d)


def _pn(v):
    return GAIN if v >= 0 else LOSS


# --------------------------------------------------------------------- builder
class Report:
    def __init__(self, a: Analysis):
        self.a = a
        self.figs: dict[str, dict] = {}
        self.sections: list[tuple[str, str, str]] = []   # (id, title, html)
        self.share = bool(getattr(a, "share", False))
        self.notional = float(getattr(a, "notional", 10_000.0))

    # -- primitives ---------------------------------------------------------
    def fig(self, fid: str, data: list, layout: dict | None = None, height: int = 320, note: str = "",
            title: str = "", subtitle: str = "", wide: bool = False) -> str:
        lay = {"height": height, "margin": {"l": 56, "r": 16, "t": 12, "b": 40}, "hovermode": "closest",
               "showlegend": len([d for d in data if d.get("showlegend", True)]) > 1}
        lay.update(layout or {})
        self.figs[fid] = {"data": data, "layout": lay}
        head = f'<h3>{html.escape(title)}</h3>' if title else ""
        sub = f'<p class="sub">{subtitle}</p>' if subtitle else ""
        foot = f'<p class="note">{note}</p>' if note else ""
        return f'<figure class="card{" wide" if wide else ""}">{head}{sub}<div class="plot" id="{fid}"></div>{foot}</figure>'

    @staticmethod
    def tiles(items: list[tuple[str, str, str]]) -> str:
        """items: (label, value, detail) - detail may contain a delta class marker."""
        out = []
        for label, value, detail in items:
            out.append(f'<div class="tile"><div class="tl"{_tip_attr(label)}>{html.escape(label)}</div>'
                       f'<div class="tv">{value}</div><div class="td">{detail}</div></div>')
        return f'<div class="tiles">{"".join(out)}</div>'

    @staticmethod
    def table(df: pd.DataFrame, fmt: dict | None = None, tid: str = "", max_rows: int | None = None,
              pnl_cols: tuple = ()) -> str:
        fmt = fmt or {}
        if df is None or df.empty:
            return '<p class="empty">No rows.</p>'
        rows = df if max_rows is None else df.head(max_rows)
        th = "".join(f'<th data-k="{i}"{_tip_attr(str(c))}>{html.escape(str(c))}</th>' for i, c in enumerate(rows.columns))
        body = []
        for _, r in rows.iterrows():
            tds = []
            for c in rows.columns:
                v = r[c]
                f = fmt.get(c)
                txt = f(v) if f else ("–" if v is None or (isinstance(v, float) and math.isnan(v)) else str(v))
                raw = v if isinstance(v, (int, float, np.integer, np.floating)) and not (isinstance(v, float) and math.isnan(v)) else txt
                cls = ""
                if c in pnl_cols and isinstance(v, (int, float, np.floating)) and not math.isnan(v):
                    cls = ' class="pos"' if v > 0 else ' class="neg"' if v < 0 else ""
                tds.append(f'<td data-v="{html.escape(str(raw))}"{cls}>{html.escape(txt)}</td>')
            body.append("<tr>" + "".join(tds) + "</tr>")
        return (f'<div class="tablewrap"><table class="sortable" id="{tid}"><thead><tr>{th}</tr></thead>'
                f'<tbody>{"".join(body)}</tbody></table></div>')

    def section(self, sid: str, title: str, body: str):
        self.sections.append((sid, title, body))

    # -- render -------------------------------------------------------------
    def to_html(self, inline_js: str | None = None) -> str:
        self.build()
        if getattr(self, "rp_views", None):
            m = self._market
            when = _when(m)
            payload = {"r": RP.R, "when": when, "is_open": m.is_open, "views": self.rp_views,
                       "est_col": "Est. now" if m.is_open else "Est. at open"}
            sid, title, body = self.sections[-1]
            self.sections[-1] = (sid, title, body + '<script type="application/json" id="rp-data">'
                                 + json.dumps(_clean(payload), allow_nan=False).replace("</", "<\\/") + '</script>')
        tpl = TEMPLATE.read_text()
        nav = "".join(f'<a href="#{sid}" data-tab="{sid}">{html.escape(t)}</a>' for sid, t, _ in self.sections)
        body = "".join(f'<section id="{sid}" class="tab"><h2>{html.escape(t)}</h2>{b}</section>'
                       for sid, t, b in self.sections)
        a = self.a
        span = ""
        if a.equity is not None:
            span = f"{a.equity.index[0]:%b %d, %Y} – {a.equity.index[-1]:%b %d, %Y}"
        if self.share:
            span += f" · shared view: $ normalized to a {money(self.notional, dp=0)} account"
        script = (f'<script>{inline_js}</script>' if inline_js else
                  f'<script src="{PLOTLY_CDN}" integrity="{PLOTLY_SRI}" crossorigin="anonymous"></script>')
        return (tpl.replace("{{NAV}}", nav).replace("{{BODY}}", body).replace("{{SPAN}}", html.escape(span))
                .replace("{{GENERATED}}", datetime.now().strftime("%Y-%m-%d" if self.share else "%Y-%m-%d %H:%M"))
                .replace("{{PLOTLY}}", script)
                .replace("{{FIGS}}", json.dumps(_clean(self.figs), default=_json_default, allow_nan=False)
                         .replace("</", "<\\/")))

    def render(self, path: str | Path, inline_js: str | None = None) -> Path:
        p = Path(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(self.to_html(inline_js))
        return p

    # ======================================================================
    def market(self):
        """Market state for re-pricing (shared by the Options and Orders tabs); None in the shared view."""
        if self.share:
            return None
        if getattr(self, "_market", None) is None:
            a = self.a
            today = datetime.now(NY).date()
            unds = {o.underlying for o in O.parse_orders(a.bundle.orders) if o.asset == "option"}
            op = a.open_positions
            if op is not None and not op.empty:
                oo = op[(op["asset"] == "option") & (pd.to_datetime(op["expiry"]).dt.date >= today)]
                unds |= set(oo["underlying"])
            snaps = a.bundle.option_snapshots
            self._market = RP.market_state(a.bundle.clock, a.bundle.stock_snapshots, unds, RP.quote_times(snaps),
                                           {u: _spot_from(a, u, None) for u in unds})
            self.rp_views = []
        return self._market

    def build(self):
        self._overview()
        self._performance()
        self._drawdown()
        self._trades()
        self._options()
        if not self.share:          # unfilled orders reveal positions and size: never in the shared view
            self._orders()
        self._positions()
        self._notes()

    # ------------------------------------------------------------- overview
    def _overview(self):
        a, st = self.a, self.a.stats
        t = a.trades
        parts = []
        if a.alerts:
            parts.append('<div class="alert wide"><b>Data coverage problem</b><ul>'
                         + "".join(f"<li>{m}</li>" for m in a.alerts) + "</ul></div>")
        if st:
            bs = a.bench_stats.get(a.primary_benchmark)
            vs = (st.total_return - bs.total_return) if bs else math.nan
            if self.share:
                parts.append('<div class="card wide prose"><b>Shared view.</b> Dollar amounts are normalized: '
                             f'account curves show the growth of {money(self.notional, dp=0)} (deposits removed); '
                             f'trade-level dollars are per {money(self.notional, dp=0)} of average account equity. '
                             'Deposits, open positions, trade sizes and intraday times are not included.</div>')
            parts.append(self.tiles([
                ("Growth of " + money(self.notional, dp=0), money(st.ending_equity, dp=0), "deposits removed")
                if self.share else
                ("Equity", money(st.ending_equity, dp=0), f"Net deposits {money(st.net_deposits, dp=0)}"),
                ("Total P&L", money(st.pnl, signed=True, dp=0), f"Time-weighted {pct(st.total_return, True)}"),
                (f"vs {a.primary_benchmark}", pct(vs, True), f"{a.primary_benchmark} {pct(bs.total_return, True) if bs else '–'}"
                 + ("" if self.share else f" · same deposits {money(bs.pnl, True, 0) if bs else '–'}")),
                ("Max drawdown", pct(st.max_dd), f"{money(st.max_dd_dollar, dp=0)} peak-to-trough P&L"),
                ("Sharpe", num(st.sharpe), f"Sortino {num(st.sortino)} · vol {pct(st.vol, dp=0)}"),
                ("Win rate", pct((t['pnl'] > 0).mean() if len(t) else math.nan, dp=0),
                 f"{len(t)} closed lots · PF {num(_profit_factor(t))}"),
            ]))
        if a.equity is not None:
            e = a.equity
            x = [_iso(d) for d in e.index]
            data = [
                {"type": "scatter", "mode": "lines", "name": "Portfolio" if self.share else "Account equity", "x": x,
                 "y": e["equity"].round(2).tolist(),
                 "line": {"color": S1, "width": 2}, "hovertemplate": "%{x}<br>Equity $%{y:,.2f}<extra></extra>"},
            ]
            if not self.share:
                data.append({"type": "scatter", "mode": "lines", "name": "Net deposits", "x": x,
                             "y": e["net_deposits"].round(2).tolist(),
                             "line": {"color": NEUTRAL, "width": 2, "shape": "hv", "dash": "dot"},
                             "hovertemplate": "%{x}<br>Deposits $%{y:,.2f}<extra></extra>"})
            for i, (sym, bf) in enumerate(a.bench.items()):
                data.append({"type": "scatter", "mode": "lines",
                             "name": sym if self.share else f"{sym} with same deposits", "x": x,
                             "y": bf["value"].round(2).tolist(), "line": {"color": [S2, S3, S4][i % 3], "width": 2},
                             "hovertemplate": f"%{{x}}<br>{sym} $%{{y:,.2f}}<extra></extra>"})
            parts.append(self.fig("f_equity", data, {"yaxis": {"tickprefix": "$"}, "hovermode": "x unified"}, 360,
                                  title=(f"Growth of {money(self.notional, dp=0)} vs. benchmarks" if self.share
                                         else "Equity vs. deposits vs. benchmarks"),
                                  subtitle=("Time-weighted: the effect of deposits is removed." if self.share else
                                            "Benchmarks buy the benchmark with every deposit on the day it lands, so "
                                            "dollar values are directly comparable."), wide=True))
            parts.append(self._waterfall())
        self.section("overview", "Overview", "".join(parts))

    def _waterfall(self) -> str:
        a = self.a
        start = a.equity.index[0]
        t = a.trades[a.trades["close_time"].dt.date >= start]
        op = a.open_positions
        cs = a.cash_summary
        items = [("Stocks realized", t.loc[t["asset"] == "stock", "pnl"].sum()),
                 ("Options realized", t.loc[t["asset"] == "option", "pnl"].sum()),
                 ("Unrealized", op["unrealized_pl"].sum(min_count=1) if not op.empty else 0.0),
                 ("Dividends", cs.get("income", 0.0)),
                 ("Margin interest", cs.get("interest", 0.0))]
        if self.share:
            items[2] = ("Unrealized", cs.get("unrealized", 0.0))
        items = [(k, float(0 if pd.isna(v) else v)) for k, v in items]
        explained = sum(v for _, v in items)
        if self.share:
            total_label, total = "Total (per avg. equity)", explained
        else:
            items.append(("Other / timing", a.stats.pnl - explained))
            total_label, total = "Account P&L", a.stats.pnl
        x = [k for k, _ in items] + [total_label]
        y = [round(v, 2) for _, v in items] + [round(total, 2)]
        data = [{"type": "waterfall", "x": x, "y": y, "measure": ["relative"] * len(items) + ["total"],
                 "connector": {"line": {"color": "var:grid", "width": 1}},
                 "increasing": {"marker": {"color": GAIN}}, "decreasing": {"marker": {"color": LOSS}},
                 "totals": {"marker": {"color": S1}}, "text": [money(v, True, 0) for v in y],
                 "textposition": "outside", "cliponaxis": False,
                 "hovertemplate": "%{x}<br>%{y:$,.2f}<extra></extra>", "showlegend": False}]
        fee_note = (f"Fees ({money(cs.get('fees', 0))}) are already inside the realized/unrealized bars: each fill's "
                    f"price is its effective price (cost ÷ quantity, fees included).")
        note = ("“Other / timing” is what the ledger can’t attribute: unpriced positions (e.g. "
                "spin-offs), mark timing and rounding.")
        if self.share:
            fee_note = (f"Per {money(self.notional, dp=0)} of average account equity, so the total differs from the "
                        f"growth-of-{money(self.notional, dp=0)} figure, which compounds returns over time. "
                        "Fees are inside the bars.")
            note = ""
        return self.fig("f_waterfall", data, {"yaxis": {"tickprefix": "$"}}, 340, title="Where the P&L came from",
                        subtitle="Since the start of the equity history. " + fee_note,
                        note=note, wide=True)

    # ---------------------------------------------------------- performance
    def _performance(self):
        a = self.a
        if a.equity is None:
            return
        e = a.equity
        x = [_iso(d) for d in e.index]
        parts = []
        data = [{"type": "scatter", "mode": "lines", "name": "Portfolio", "x": x, "y": (e["twr"] - 1).round(5).tolist(),
                 "line": {"color": S1, "width": 2}}]
        for i, (sym, bf) in enumerate(a.bench.items()):
            data.append({"type": "scatter", "mode": "lines", "name": sym, "x": x, "y": (bf["twr"] - 1).round(5).tolist(),
                         "line": {"color": [S2, S3, S4][i % 3], "width": 2}})
        parts.append(self.fig("f_twr", data, {"yaxis": {"tickformat": "+.0%"}, "hovermode": "x unified"}, 340,
                              title="Time-weighted return", subtitle="Deposits removed (Modified Dietz, daily) – "
                              "this is the skill-only comparison against simply holding the index.", wide=True))

        data = [{"type": "scatter", "mode": "lines", "name": "Your cumulative P&L", "x": x,
                 "y": e["cum_pnl"].round(2).tolist(), "line": {"color": S1, "width": 2}}]
        for i, (sym, bf) in enumerate(a.bench.items()):
            data.append({"type": "scatter", "mode": "lines", "name": f"{sym} buy & hold P&L", "x": x,
                         "y": bf["cum_pnl"].round(2).tolist(), "line": {"color": [S2, S3, S4][i % 3], "width": 2}})
        parts.append(self.fig("f_cumpnl", data, {"yaxis": {"tickprefix": "$"}, "hovermode": "x unified"}, 320,
                              title="Cumulative P&L in dollars vs. buy & hold"))

        data = [{"type": "bar", "x": x, "y": e["pnl"].round(2).tolist(), "name": "Daily P&L",
                 "marker": {"color": [_pn(v) for v in e["pnl"]]},
                 "hovertemplate": "%{x}<br>%{y:$,.2f}<extra></extra>", "showlegend": False}]
        parts.append(self.fig("f_daily", data, {"yaxis": {"tickprefix": "$"}, "bargap": 0.15}, 280,
                              title="Daily P&L", subtitle=f"{pct(a.stats.pct_up_days, dp=0)} of days were up · best "
                              f"{pct(a.stats.best_day, True)} · worst {pct(a.stats.worst_day, True)}"))

        m = a.monthly
        if m is not None and len(m):
            mx = [f"{d:%b %Y}" for d in m.index]
            data = [{"type": "bar", "name": "Portfolio", "x": mx, "y": m["ret"].round(5).tolist(),
                     "marker": {"color": S1}, "hovertemplate": "%{x}<br>%{y:+.1%}<extra>Portfolio</extra>"}]
            pb = a.bench.get(a.primary_benchmark)
            if pb is not None:
                br = (1 + pb["ret"].set_axis(pd.to_datetime(pd.Index(pb.index)))).resample("ME").prod() - 1
                data.append({"type": "bar", "name": a.primary_benchmark, "x": mx,
                             "y": br.reindex(m.index).round(5).tolist(), "marker": {"color": S2},
                             "hovertemplate": f"%{{x}}<br>%{{y:+.1%}}<extra>{a.primary_benchmark}</extra>"})
            parts.append(self.fig("f_monthly", data, {"yaxis": {"tickformat": "+.0%"}, "barmode": "group",
                                                      "bargap": 0.3, "bargroupgap": 0.08}, 280,
                                  title="Monthly returns"))

        pb = a.bench.get(a.primary_benchmark)
        if pb is not None:
            r = e["ret"].iloc[1:]
            b = pb["ret"].reindex(r.index).fillna(0)
            st = a.stats
            xs = np.linspace(b.min(), b.max(), 20)
            data = [{"type": "scatter", "mode": "markers", "name": "Days", "x": b.round(5).tolist(), "y": r.round(5).tolist(),
                     "text": [_iso(d) for d in r.index], "marker": {"color": S1, "size": 8, "opacity": 0.75,
                                                                    "line": {"color": "var:surface", "width": 1}},
                     "hovertemplate": "%{text}<br>" + a.primary_benchmark + " %{x:+.2%}<br>You %{y:+.2%}<extra></extra>"},
                    {"type": "scatter", "mode": "lines", "name": f"β = {num(st.beta)}", "x": xs.tolist(),
                     "y": (st.alpha / 252 + st.beta * xs).tolist(), "line": {"color": S2, "width": 2}}]
            parts.append(self.fig("f_beta", data, {"xaxis": {"tickformat": "+.1%", "title": {"text": f"{a.primary_benchmark} daily return"}},
                                                   "yaxis": {"tickformat": "+.0%", "title": {"text": "Your daily return"}}}, 340,
                                  title=f"Daily returns vs. {a.primary_benchmark}",
                                  subtitle=f"β {num(st.beta)} · correlation {num(st.correlation)} · annualized α "
                                           f"{pct(st.alpha, True)} · up-capture {pct(st.up_capture, dp=0)} · "
                                           f"down-capture {pct(st.down_capture, dp=0)}"))

        ro = a.rolling
        if ro is not None and ro["vol"].notna().sum() > 2:
            data = [{"type": "scatter", "mode": "lines", "name": "Rolling vol (21d, ann.)", "x": x,
                     "y": [_num(v) for v in ro["vol"].round(4)], "line": {"color": S1, "width": 2}, "showlegend": False}]
            parts.append(self.fig("f_rvol", data, {"yaxis": {"tickformat": ".0%"}}, 240,
                                  title="Rolling 21-day volatility (annualized)"))
            data = [{"type": "scatter", "mode": "lines", "name": "Rolling Sharpe", "x": x,
                     "y": [_num(v) for v in ro["sharpe"].round(3)], "line": {"color": S1, "width": 2}, "showlegend": False}]
            parts.append(self.fig("f_rsharpe", data, {}, 240, title="Rolling 21-day Sharpe"))
            if "beta" in ro:
                data = [{"type": "scatter", "mode": "lines", "name": "Rolling beta", "x": x,
                         "y": [_num(v) for v in ro["beta"].round(3)], "line": {"color": S1, "width": 2}, "showlegend": False}]
                parts.append(self.fig("f_rbeta", data, {}, 240, title=f"Rolling 21-day beta to {a.primary_benchmark}"))

        rows = []
        for name, s in [("Portfolio", a.stats)] + [(k, v) for k, v in a.bench_stats.items()]:
            rows.append({"": name, "Total return": pct(s.total_return, True), "Annualized": pct(s.cagr, True),
                         "Volatility": pct(s.vol), "Sharpe": num(s.sharpe), "Sortino": num(s.sortino),
                         "Max drawdown": pct(s.max_dd), "Calmar": num(s.calmar),
                         ("P&L on " + money(self.notional, dp=0) if self.share else "P&L on your deposits"): money(s.pnl, True)})
        parts.append('<figure class="card wide"><h3>Risk & return summary</h3>'
                     '<p class="sub">Benchmarks are run with your exact deposit dates and amounts. '
                     'Short histories make annualized numbers noisy – read them as direction, not forecast.</p>'
                     + self.table(pd.DataFrame(rows), tid="t_riskret") + '</figure>')
        self.section("performance", "Performance", "".join(parts))

    # ------------------------------------------------------------- drawdown
    def _drawdown(self):
        a = self.a
        if a.equity is None:
            return
        e = a.equity
        x = [_iso(d) for d in e.index]
        data = [{"type": "scatter", "mode": "lines", "name": "Portfolio", "x": x, "y": e["dd"].round(5).tolist(),
                 "fill": "tozeroy", "line": {"color": LOSS, "width": 2}, "fillcolor": "var:loss-fill"}]
        for i, (sym, bf) in enumerate(a.bench.items()):
            data.append({"type": "scatter", "mode": "lines", "name": sym, "x": x, "y": bf["dd"].round(5).tolist(),
                         "line": {"color": [S2, S3, S4][i % 3], "width": 2}})
        dd_now = float(e["dd"].iloc[-1])
        peak_d = e["twr"].idxmax()
        deepest = float(e["dd"].min())
        parts = [self.fig("f_underwater", data, {"yaxis": {"tickformat": ".0%"}, "hovermode": "x unified"}, 320,
                          title="Underwater curve (time-weighted)",
                          subtitle="How far the deposit-neutral return index is below its own best level so far — "
                                   "a fall from the peak, not a loss on your deposits. "
                                   + (f"Now {pct(dd_now, dp=1)} below the {peak_d:%b} {peak_d.day} peak" if dd_now < -1e-9
                                      else "At a new high")
                                   + f"; deepest {pct(deepest, dp=1)}.", wide=True)]
        cum = e["cum_pnl"]
        hwm = cum.cummax()
        gap = cum - hwm
        data = [{"type": "scatter", "mode": "lines", "name": "Peak so far (high-water mark)", "x": x,
                 "y": hwm.round(2).tolist(), "line": {"color": NEUTRAL, "width": 1.5, "dash": "dot", "shape": "hv"},
                 "hovertemplate": "peak %{y:$,.2f}<extra></extra>"},
                {"type": "scatter", "mode": "lines", "name": "Cumulative P&L", "x": x, "y": cum.round(2).tolist(),
                 "fill": "tonexty", "fillcolor": "var:loss-fill", "line": {"color": S1, "width": 2},
                 "customdata": gap.round(2).tolist(),
                 "hovertemplate": "P&L %{y:$,.2f}<br>%{customdata:$,.2f} vs. the peak<extra></extra>"}]
        now, top, top_d = float(cum.iloc[-1]), float(hwm.iloc[-1]), cum.idxmax()
        sub = (f"Now {money(now, True, 0)} on your deposits"
               + (f", {money(top - now, dp=0)} below the peak of {money(top, True, 0)} on {top_d:%b} {top_d.day}"
                  if top - now > 0.5 else ", at its peak")
               + ". The shaded gap is the drawdown: the distance from the best level reached so far.")
        parts.append(self.fig("f_dd_dollar", data, {"yaxis": {"tickprefix": "$"}, "hovermode": "x unified",
                                                    "shapes": [{"type": "line", "xref": "paper", "x0": 0, "x1": 1,
                                                                "y0": 0, "y1": 0, "line": {"color": "var:axis",
                                                                                           "width": 1}}]}, 300,
                              title="Cumulative P&L and its high-water mark", subtitle=sub))
        dd = a.drawdowns
        if dd is not None and not dd.empty:
            show = dd.assign(peak=dd["peak"].map(_iso), trough=dd["trough"].map(_iso),
                             recovered=dd["recovered"].map(lambda v: _iso(v) if v is not None and not pd.isna(v) else "not yet"))
            show = show.rename(columns={"peak": "Peak", "trough": "Trough", "recovered": "Recovered",
                                        "depth": "Depth", "length_days": "Days"})
            parts.append('<figure class="card"><h3>Worst drawdowns</h3>'
                         + self.table(show, {"Depth": pct}, tid="t_dd") + '</figure>')
        self.section("drawdown", "Drawdown", "".join(parts))

    # --------------------------------------------------------------- trades
    def _trades(self):
        a = self.a
        if a.trades.empty:
            return
        t = a.trades.sort_values("close_time")
        parts = []
        w, l = t[t["pnl"] > 0], t[t["pnl"] < 0]
        streak_w, streak_l = _streaks(t["pnl"])
        parts.append(self.tiles([
            ("Realized P&L", money(t["pnl"].sum(), True, 0),
             f"{len(t)} closed lots" + ("" if self.share else f" · fees {money(t['fees'].sum())}")),
            ("Win rate", pct(len(w) / len(t), dp=0), f"{len(w)} wins / {len(l)} losses"),
            ("Avg win / loss", f"{money(w['pnl'].mean(), dp=0)} / {money(l['pnl'].mean(), dp=0)}",
             f"payoff ratio {num(abs(w['pnl'].mean() / l['pnl'].mean()) if len(l) and len(w) else math.nan)}"),
            ("Profit factor", num(_profit_factor(t)), f"expectancy {money(t['pnl'].mean(), True)} per lot"),
            ("Largest win / loss", f"{money(t['pnl'].max(), dp=0)} / {money(t['pnl'].min(), dp=0)}",
             f"streaks: {streak_w} wins · {streak_l} losses"),
            ("Median hold", _dur(t["hold_days"].median()), f"mean {_dur(t['hold_days'].mean())}"),
        ]))

        data = []
        for i, (asset, col) in enumerate([("stock", S1), ("option", S2)]):
            g = t[t["asset"] == asset]
            if len(g):
                data.append({"type": "scatter", "mode": "lines", "name": "Stocks" if asset == "stock" else "Options",
                             "x": [_iso(v) for v in g["close_time"]], "y": g["pnl"].cumsum().round(2).tolist(),
                             "line": {"color": col, "width": 2, "shape": "hv"}})
        data.append({"type": "scatter", "mode": "lines", "name": "All", "x": [_iso(v) for v in t["close_time"]],
                     "y": t["pnl"].cumsum().round(2).tolist(), "line": {"color": S3, "width": 2, "shape": "hv"}})
        parts.append(self.fig("f_cumreal", data, {"yaxis": {"tickprefix": "$"}}, 320, title="Cumulative realized P&L",
                              subtitle="Stepped at each close; FIFO lot matching, fees included.", wide=True))

        bins = _nice_bins(t["pnl"])
        counts, edges = np.histogram(t["pnl"], bins=bins)
        centers = (edges[:-1] + edges[1:]) / 2
        data = [{"type": "bar", "x": centers.round(2).tolist(), "y": counts.tolist(), "width": (np.diff(edges) * 0.9).tolist(),
                 "marker": {"color": [_pn(c) for c in centers]}, "showlegend": False,
                 "customdata": np.c_[edges[:-1], edges[1:]].round(0).tolist(),
                 "hovertemplate": "$%{customdata[0]:,} to $%{customdata[1]:,}<br>%{y} lots<extra></extra>"}]
        parts.append(self.fig("f_hist", data, {"xaxis": {"tickprefix": "$"}, "yaxis": {"title": {"text": "Lots"}}}, 280,
                              title="P&L distribution per closed lot"))

        # Per-trade points carry a symbol and date, so the shared view shows return % rather than dollars
        # (a dollar figure next to a known contract price would reveal the normalization factor).
        ycol, yfmt, hfmt = ("ret", {"tickformat": "+.0%"}, "%{y:+.1%}") if self.share else \
            ("pnl", {"tickprefix": "$"}, "%{y:$,.2f}")
        data = []
        for asset, col in [("stock", S1), ("option", S2)]:
            g = t[t["asset"] == asset]
            if len(g):
                data.append({"type": "scatter", "mode": "markers", "name": "Stocks" if asset == "stock" else "Options",
                             "x": g["hold_days"].clip(lower=1 / 1440).round(4).tolist(), "y": g[ycol].round(4).tolist(),
                             "text": g["symbol"].tolist(), "marker": {"color": col, "size": 9, "opacity": 0.8,
                                                                       "line": {"color": "var:surface", "width": 1}},
                             "hovertemplate": "%{text}<br>held %{x:.2f} d<br>" + hfmt + "<extra></extra>"})
        parts.append(self.fig("f_hold", data, {"xaxis": {"type": "log", "title": {"text": "Days held (log)"}},
                                               "yaxis": yfmt}, 300,
                              title="Holding period vs. " + ("return" if self.share else "P&L")))

        sy = a.symbols
        if sy is not None and not sy.empty and "realized" in sy:
            # Trades tab = closed positions only: realized P&L of closed lots, no marks, no dividends
            s2 = sy[sy["trades"] > 0].sort_values("realized")
            data = [{"type": "bar", "orientation": "h", "y": s2.index.tolist(), "x": s2["realized"].round(2).tolist(),
                     "marker": {"color": [_pn(v) for v in s2["realized"]]}, "showlegend": False,
                     "customdata": np.c_[s2["stock_pnl"], s2["option_pnl"], s2["trades"]].round(2).tolist(),
                     "hovertemplate": "<b>%{y}</b><br>Realized %{x:$,.2f}<br>Stock %{customdata[0]:$,.2f}"
                                      "<br>Options %{customdata[1]:$,.2f}<br>%{customdata[2]} closed lots"
                                      "<extra></extra>"}]
            n_open = int(s2["is_open"].sum())
            parts.append(self.fig("f_bysym", data, {"xaxis": {"tickprefix": "$"}, "margin": {"l": 84}},
                                  max(300, 22 * len(s2) + 60), title="Realized P&L by underlying",
                                  subtitle="Closed lots only (stock + options, fees included). Open positions and "
                                           "dividends are excluded"
                                           + ("" if self.share or not n_open else
                                              f"; {n_open} of these names still have open positions, shown on the "
                                              "Options and Positions tabs") + "."))
            if "bhc_pnl" in s2 and s2["bhc_pnl"].notna().any():
                s3 = s2[s2["bhc_pnl"].notna()].sort_values("edge_closed")
                data = [{"type": "bar", "orientation": "h", "name": "Your realized P&L", "y": s3.index.tolist(),
                         "x": s3["realized"].round(2).tolist(), "marker": {"color": S1}},
                        {"type": "bar", "orientation": "h", "name": "Buy & hold P&L", "y": s3.index.tolist(),
                         "x": s3["bhc_pnl"].round(2).tolist(), "marker": {"color": S2},
                         "customdata": np.c_[s3["bhc_capital"], s3["bhc_return"]].tolist(),
                         "text": [str(d) for d in s3["bhc_end"]],
                         "hovertemplate": "%{y}: %{x:$,.2f}<br>%{customdata[0]:$,.0f} held from first trade to "
                                          "your last close %{text} (%{customdata[1]:+.1%})<extra>Buy & hold</extra>"}]
                parts.append(self.fig("f_edge", data, {"barmode": "group", "xaxis": {"tickprefix": "$"},
                                                       "margin": {"l": 84}, "bargap": 0.25},
                                      max(300, 30 * len(s3) + 80), title="Your trading vs. just holding the underlying",
                                      subtitle="Realized P&L of closed lots vs. buy & hold over the same span: your "
                                               "peak capital committed to that name up to your last closed lot, bought "
                                               "on your first trade date and sold on that last close. Positions still "
                                               "open are not counted. Sorted by your edge."))

        # timing: totals are easily decided by one big trade, so show counts and medians alongside
        tt = t.assign(wd=t["open_time"].dt.dayofweek, hr=t["open_time"].dt.hour)
        days = sorted(set(range(5)) | set(tt["wd"].unique()))
        names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        parts.append(self._bucket_fig("f_wd", tt, "wd", days, [names[d] for d in days],
                                      "Realized P&L by entry weekday", "", self.share))
        if not self.share:  # intraday timing is omitted from shared reports
            hrs = sorted(tt["hr"].unique())
            parts.append(self._bucket_fig("f_hr", tt, "hr", hrs, [f"{h:02d}:00" for h in hrs],
                                          "Realized P&L by entry hour (ET)", "Includes overnight-session entries. ", False,
                                          ticks=[str(h) for h in hrs]))

        # gantt
        g = t.copy()
        g["y"] = g["underlying"]
        order = g.groupby("y")["open_time"].min().sort_values(ascending=False).index.tolist()
        data = []
        for sign, col, name in [(1, GAIN, "Winning lot"), (-1, LOSS, "Losing lot")]:
            gg = g[(g["pnl"] >= 0) if sign > 0 else (g["pnl"] < 0)]
            xs, ys, tx = [], [], []
            for _, r in gg.iterrows():
                xs += [_iso(r["open_time"]), _iso(r["close_time"]), None]
                ys += [r["y"], r["y"], None]
                size = "" if self.share else f" ×{r['qty']:g}"
                outcome = pct(r["ret"], True) if self.share else money(r["pnl"], True)
                lab = f"{r['symbol']} {r['direction']}{size}: {outcome}"
                tx += [lab, lab, None]
            data.append({"type": "scatter", "mode": "lines+markers", "name": name, "x": xs, "y": ys, "text": tx,
                         "line": {"color": col, "width": 5}, "marker": {"size": 8, "color": col},
                         "hovertemplate": "%{text}<extra></extra>", "connectgaps": False})
        parts.append(self.fig("f_gantt", data, {"yaxis": {"categoryorder": "array", "categoryarray": order},
                                                "margin": {"l": 84}}, max(320, 20 * len(order) + 80),
                              title="Trade timeline", subtitle="Each bar is one closed lot from entry to exit.",
                              wide=True))

        show = t[["close_time", "symbol", "asset", "direction", "qty", "open_price", "close_price", "close_kind",
                  "hold_days", "fees", "pnl", "ret"]].sort_values("close_time", ascending=False).copy()
        show["close_time"] = show["close_time"].map(_iso)
        show.columns = ["Closed", "Symbol", "Asset", "Side", "Qty", "Entry (eff.)", "Exit (eff.)", "How closed",
                        "Days", "Fees", "P&L", "Return"]
        if self.share:
            # prices + scaled P&L would reveal the scale factor (most option lots are 1 contract)
            show = show.drop(columns=["Qty", "Entry (eff.)", "Exit (eff.)", "Fees", "P&L"])
        sub = ("Click a header to sort. Return is on the lot's fee-inclusive cost; per-lot dollar amounts are "
               "omitted in the shared view." if self.share else
               "Click a header to sort. Entry/exit are effective prices: net cash ÷ (qty × multiplier), fees included.")
        parts.append(f'<figure class="card wide"><h3>All closed lots</h3><p class="sub">{sub}</p>'
                     + self.table(show, {"Qty": lambda v: f"{v:g}", "Entry (eff.)": lambda v: num(v, 4),
                                         "Exit (eff.)": lambda v: num(v, 4), "Days": lambda v: num(v, 1),
                                         "Fees": money, "P&L": lambda v: money(v, True), "Return": lambda v: pct(v, True)},
                                  tid="t_trades", pnl_cols=("P&L", "Return")) + '</figure>')
        self.section("trades", "Trades", "".join(parts))

    def _bucket_fig(self, fid, t, key, keys, labels, title, sub, share, ticks=None) -> str:
        """Trades-tab timing charts: one bucket per value of ``t[key]``."""
        groups = [(lab, tk, t.loc[t[key] == k, "pnl"]) for k, lab, tk in zip(keys, labels, ticks or labels)]
        return self._dist_fig(fid, groups, title, sub, unit="lots", hover_suffix=" ET" if ticks else "",
                              axis_title="number of closed lots under each label"
                                         + (" · hour of entry, ET" if ticks else ""))

    def _dist_fig(self, fid, groups, title, sub="", horizontal=False, unit="lots", hover_suffix="",
                  axis_title="", height=None) -> str:
        """Total P&L per group (bars), the group's size in its label, and the median item (diamonds on a second
        axis whose zero lines up with the bars'). ``groups``: [(label, tick label, pnl Series)]."""
        rows = []
        for lab, tk, p in groups:
            p = pd.Series(p, dtype=float).dropna()
            n = len(p)
            rows.append({"lab": lab, "tk": tk, "n": n, "sum": float(p.sum()) if n else 0.0,
                         "median": float(p.median()) if n else None, "win": float((p > 0).mean()) if n else None,
                         "best": float(p.max()) if n else None, "worst": float(p.min()) if n else None,
                         "ex_worst": float(p.sum() - p.min()) if n > 1 and p.min() < 0 else None})

        def fmt(v):
            return "–" if v is None or pd.isna(v) else money(v, True)

        labels = [r["lab"] for r in rows]
        sums = [round(r["sum"], 2) for r in rows]
        meds = [None if r["median"] is None else round(r["median"], 2) for r in rows]
        cd = [[r["n"], "–" if r["win"] is None else f"{r['win']:.0%}", fmt(r["median"]), fmt(r["best"]),
               fmt(r["worst"]), fmt(r["ex_worst"]) if r["ex_worst"] is not None else "n/a", r["lab"] + hover_suffix]
              for r in rows]
        v = "x" if horizontal else "y"
        ht = (f"<b>%{{customdata[6]}}</b>: total %{{{v}:$,.2f}} over %{{customdata[0]}} {unit}<br>win rate "
              "%{customdata[1]} · median %{customdata[2]}<br>best %{customdata[3]} · worst %{customdata[4]}<br>"
              f"total without the worst one %{{customdata[5]}}<extra></extra>")
        bar = {"type": "bar", "name": "Total", "customdata": cd, "marker": {"color": [_pn(x) for x in sums]},
               "hovertemplate": ht}
        dia = {"type": "scatter", "mode": "markers", "name": f"Median {unit[:-1] if unit.endswith('s') else unit}",
               "marker": {"color": "var:ink", "symbol": "diamond", "size": 8, "line": {"width": 1, "color": "var:surface"}},
               "customdata": cd,
               "hovertemplate": f"<b>%{{customdata[6]}}</b>: median %{{{v}:$,.2f}} (%{{customdata[0]}} {unit})<extra></extra>"}
        if horizontal:
            bar.update(orientation="h", y=labels, x=sums)
            dia.update(y=labels, x=meds, xaxis="x2")
        else:
            bar.update(x=labels, y=sums)
            dia.update(x=labels, y=meds, yaxis="y2")
        # both zero lines at the same place: the median axis is the total axis scaled by k
        lo, hi = min(0.0, min(sums)), max(0.0, max(sums))
        span = (hi - lo) or 1.0
        lo, hi = min(lo - 0.06 * span, -0.08 * span), max(hi + 0.06 * span, 0.08 * span)
        mv = [m for m in meds if m is not None]
        k = max([m / hi for m in mv if m > 0] + [m / lo for m in mv if m < 0] + [0.0]) * 1.1 or 1.0
        main = {"tickprefix": "$", "range": [lo, hi], "title": {"text": "total"}}
        second = {"overlaying": v, "tickprefix": "$", "range": [lo * k, hi * k], "showgrid": False, "zeroline": False,
                  "title": {"text": "median"}}
        if horizontal:
            ticktext = [f"{r['tk']} <span style='font-size:10px'>· {r['n']}</span>" for r in rows]
            cat = {"tickmode": "array", "tickvals": labels, "ticktext": ticktext}
            layout = {"xaxis": main, "xaxis2": {**second, "side": "top"}, "yaxis": cat,
                      "margin": {"l": 180, "t": 40}}
            h = height or max(280, 30 * len(rows) + 110)
        else:
            ticktext = [f"{r['tk']}<br><span style='font-size:10px'>{r['n']}</span>" for r in rows]
            cat = {"tickmode": "array", "tickvals": labels, "ticktext": ticktext, "tickangle": 0,
                   "title": {"text": axis_title or f"number of {unit} under each label"}}
            layout = {"xaxis": cat, "yaxis": main, "yaxis2": {**second, "side": "right"}, "margin": {"r": 56},
                      "bargap": 0.3 if len(rows) > 8 else 0.35}
            h = height or 380
        layout.update({"showlegend": True, "legend": {"orientation": "h", "y": 1.02, "yanchor": "bottom", "x": 1,
                                                       "xanchor": "right"}})
        if horizontal:   # the legend names the two axes; titles would collide with it
            main["title"] = {"text": ""}
            layout["xaxis2"]["title"] = {"text": ""}
            layout["legend"] = {"orientation": "h", "y": -0.06, "yanchor": "top", "x": 1, "xanchor": "right"}
            layout["xaxis2"]["ticksuffix"] = " median"
            layout["xaxis"]["ticksuffix"] = ""
        return self.fig(fid, [bar, dia], layout, h, title=title,
                        subtitle=(sub + " " if sub else "") + f"Bars: total, with the number of {unit} by each "
                                 "label. Diamonds: the median (second axis); when it disagrees with the bar, a few "
                                 "big results decide the total.")

    # -------------------------------------------------------------- options
    def _options(self):
        a = self.a
        c = a.combos
        if c is None or c.empty:
            return
        parts = []
        done = c[~c["is_open"]]
        wins = done[done["pnl"] > 0]
        parts.append(self.tiles([
            ("Options P&L", money(c["pnl"].sum(), True, 0),
             f"realized {money(c['realized_pnl'].sum(), True, 0)} · open {money(c['unrealized_pnl'].sum(), True, 0)}"),
            ("Strategies", str(len(c)), f"{len(done)} closed · {int(c['is_open'].sum())} open · "
                                        f"{int(c['rolled_from'].notna().sum())} rolls"),
            ("Win rate", pct(len(wins) / len(done) if len(done) else math.nan, dp=0),
             f"avg win {money(wins['pnl'].mean(), dp=0)} · avg loss {money(done[done['pnl'] <= 0]['pnl'].mean(), dp=0)}"),
            ("Median return on risk", pct(done["ror"].median(), True, 0), "defined-risk positions only"),
            ("Premium", f"{money(-c.loc[c['net_premium'] < 0, 'net_premium'].sum(), dp=0)} paid",
             f"{money(c.loc[c['net_premium'] > 0, 'net_premium'].sum(), dp=0)} collected"),
            ("Median DTE at entry", f"{c['dte'].median():.0f} d", f"median hold {_dur(done['hold_days'].median())}"),
        ]))

        open_note = "Open strategies count with their unrealized P&L."
        by = c.groupby("strategy")["pnl"].sum().sort_values()
        parts.append(self._dist_fig("f_strat", [(k, k, c.loc[c["strategy"] == k, "pnl"]) for k in by.index],
                                    "P&L by strategy", "Legs opened within the grouping window on one underlying form "
                                    "a strategy; covered calls are detected from shares held at entry. " + open_note,
                                    horizontal=True, unit="trades"))

        # cumulative by strategy (top 5 + other)
        top = c.groupby("strategy")["pnl"].apply(lambda s: s.abs().sum()).sort_values(ascending=False).index[:5]
        cc = c.assign(grp=np.where(c["strategy"].isin(top), c["strategy"], "Other"),
                      when=pd.to_datetime(c["close_time"]).fillna(pd.Timestamp.now(tz=NY))).sort_values("when")
        data = []
        cols = [S1, S2, S3, S4, S5, S6]
        for i, grp in enumerate(list(top) + (["Other"] if (cc["grp"] == "Other").any() else [])):
            g = cc[cc["grp"] == grp]
            data.append({"type": "scatter", "mode": "lines", "name": grp, "x": [_iso(v) for v in g["when"]],
                         "y": g["pnl"].cumsum().round(2).tolist(), "line": {"color": cols[i], "width": 2, "shape": "hv"}})
        parts.append(self.fig("f_stratcum", data, {"yaxis": {"tickprefix": "$"}}, 320,
                              title="Cumulative P&L by strategy", wide=True))

        def bucket_chart(fid, col, edges, labels, title, sub=""):
            b = pd.cut(c[col], edges, labels=labels, right=True, include_lowest=True)
            return self._dist_fig(fid, [(lab, lab, c.loc[b == lab, "pnl"]) for lab in labels], title, sub,
                                  unit="trades", height=340)

        parts.append(bucket_chart("f_dte", "dte", [-1, 0, 2, 7, 14, 30, 60, 10_000],
                                  ["0DTE", "1–2", "3–7", "8–14", "15–30", "31–60", "60+"],
                                  "P&L by days-to-expiration at entry", open_note))
        parts.append(bucket_chart("f_holdopt", "hold_days", [-1, 1 / 24, 1, 3, 7, 21, 10_000],
                                  ["<1h", "1h–1d", "1–3d", "3–7d", "1–3w", "3w+"], "P&L by time in trade",
                                  "Open strategies count the time held so far."))

        kind = np.where(c["rights"] == "C", "Calls", np.where(c["rights"] == "P", "Puts", "Calls + puts"))
        cp = [(k, k, c.loc[kind == k, "pnl"]) for k in ("Calls", "Puts", "Calls + puts") if (kind == k).any()]
        cp += [(f"{k} trades", f"{k} trades", c.loc[c["premium_side"] == k, "pnl"])
               for k in ("Debit", "Credit") if (c["premium_side"] == k).any()]
        parts.append(self._dist_fig("f_cp", cp, "Calls vs. puts · debit vs. credit", open_note, unit="trades",
                                    height=340))

        oc = c["outcome"].value_counts()
        parts.append(self._dist_fig("f_outcome", [(k, k, c.loc[c["outcome"] == k, "pnl"]) for k in oc.index],
                                    "How positions ended", unit="trades", height=340))

        ou = c.groupby("underlying")["pnl"].sum().sort_values()
        parts.append(self._dist_fig("f_optund", [(k, k, c.loc[c["underlying"] == k, "pnl"]) for k in ou.index],
                                    "Options P&L by underlying", open_note, horizontal=True, unit="trades"))
        if not self.share:   # share counts would reveal the account's dollar size
            parts.append(self._reinvest_card())

        rr = done["ror"].dropna()
        if len(rr) > 2:
            data = [{"type": "histogram", "x": rr.clip(-1, 3).round(4).tolist(), "xbins": {"size": 0.25},
                     "marker": {"color": S1, "line": {"color": "var:surface", "width": 2}}, "showlegend": False,
                     "hovertemplate": "%{x}: %{y} trades<extra></extra>"}]
            parts.append(self.fig("f_ror", data, {"xaxis": {"tickformat": "+.0%", "title": {"text": "P&L ÷ max risk"}},
                                                  "yaxis": {"title": {"text": "Trades"}}}, 280,
                                  title="Return on risk distribution",
                                  subtitle="−100% = full loss of the premium or spread width at risk."))

        if c["moneyness"].notna().sum() > 3:
            m = c[c["moneyness"].notna()]
            my, myf, mh = ("ror", {"tickformat": "+.0%"}, "%{y:+.0%}") if self.share else \
                ("pnl", {"tickprefix": "$"}, "%{y:$,.2f}")
            data = [{"type": "scatter", "mode": "markers", "x": m["moneyness"].round(4).tolist(), "y": m[my].round(4).tolist(),
                     "text": m["legs"].tolist(), "marker": {"color": [_pn(v) for v in m["pnl"]], "size": 9},
                     "showlegend": False, "hovertemplate": "%{text}<br>moneyness %{x:+.1%}<br>" + mh + "<extra></extra>"}]
            parts.append(self.fig("f_money", data, {"xaxis": {"tickformat": "+.0%", "title": {"text": "Moneyness at entry (+ = ITM)"}},
                                                    "yaxis": myf}, 300, title="Single-leg P&L vs. moneyness at entry"))

        # rolls
        camp = c[c["campaign"].isin(c.loc[c["rolled_from"].notna(), "campaign"])]
        if not camp.empty:
            cg = camp.groupby("campaign").agg(Underlying=("underlying", "first"), Legs=("combo_id", "size"),
                                              Start=("open_time", "min"), PnL=("pnl", "sum"),
                                              Path=("legs", lambda s: "  →  ".join(s)))
            cg["Start"] = cg["Start"].map(_iso)
            cg = cg.rename(columns={"Legs": "Positions", "PnL": "Total P&L"})
            if self.share:
                cg["Total P&L"] = np.sign(cg["Total P&L"]).map({1.0: "gain", -1.0: "loss", 0.0: "flat"})
            parts.append('<figure class="card wide"><h3>Roll chains</h3><p class="sub">A position opened within the '
                         'window of closing another on the same underlying is treated as a roll.</p>'
                         + self.table(cg.reset_index(drop=True),
                                      {} if self.share else {"Total P&L": lambda v: money(v, True)}, tid="t_rolls",
                                      pnl_cols=("Total P&L",)) + '</figure>')

        parts.append(self._open_options())

        show = c[["open_time", "underlying", "strategy", "legs", "dte", "net_premium", "max_risk", "outcome",
                  "hold_days", "pnl", "ror"]].sort_values("open_time", ascending=False).copy()
        show["open_time"] = show["open_time"].map(_iso)
        show.columns = ["Opened", "Underlying", "Strategy", "Legs", "DTE", "Net premium", "Max risk", "Outcome",
                        "Days", "P&L", "Return on risk"]
        sub = ("Net premium: + credit received / − debit paid, fees included. Open positions include unrealized P&L.")
        if self.share:
            show = show.drop(columns=["Net premium", "Max risk", "P&L"])
            sub = "Return on risk = P&L ÷ max risk at entry. Per-position dollar amounts are omitted in the shared view."
        parts.append(f'<figure class="card wide"><h3>All option strategies</h3><p class="sub">{sub}</p>'
                     + self.table(show, {"Net premium": lambda v: money(v, True), "Max risk": money,
                                         "Days": lambda v: num(v, 1), "P&L": lambda v: money(v, True),
                                         "Return on risk": lambda v: pct(v, True, 0)},
                                  tid="t_strats", pnl_cols=("P&L", "Return on risk")) + '</figure>')
        self.section("options", "Options", "".join(parts))

    def _reinvest_card(self) -> str:
        """Whole shares of each underlying that the option profits would have bought."""
        a = self.a
        op = a.open_positions
        held = (op[op["asset"] == "stock"].groupby("symbol")["qty"].sum().to_dict()
                if op is not None and not op.empty else {})
        df = reinvest_table(a.combos, a.bundle.bars, lambda u: _spot_from(a, u, None), held)
        if df.empty:
            return ""
        df = df[(df["profits"] > 0) | (df["held"] != 0)]
        if df.empty:
            return ""

        def diff(v):
            if pd.isna(v) or abs(v) < 1e-9:
                return "on plan"
            return f"buy {v:g}" if v > 0 else f"{-v:g} extra"

        show = pd.DataFrame({
            "Underlying": df["underlying"],
            "Closed (W/L)": [f"{w} / {l}" for w, l in zip(df["wins"], df["losses"])],
            "Profits from wins": df["profits"], "Shares": df["shares"], "Shares (L)": df["net_shares"],
            "Avg cost": df["avg_cost"], "Cash left": df["cash"], "Cash left (L)": df["net_cash"],
            "Price now": df["price_now"], "Value now": df["value"].where(df["shares"] > 0),
            "Gain on shares": df["gain"], "You hold": df["held"], "Vs. plan": df["short_by"],
        })
        to_buy = df[df["short_by"] > 0]
        buy_cost = float((to_buy["short_by"] * to_buy["price_now"]).sum(skipna=True))
        tot_cost, tot_val = float(df["cost"].sum()), float(df["value"].sum(skipna=True))
        unpriced = int(df["unpriced"].sum())
        sub = (f"Profits from {int(df['wins'].sum())} winning positions ({money(df['profits'].sum(), dp=0)}) would have "
               f"bought shares costing {money(tot_cost, dp=0)}, worth {money(tot_val, dp=0)} now "
               f"({money(tot_val - tot_cost, True, 0)}). Matching the plan means buying "
               f"{int(to_buy['short_by'].sum())} shares across {len(to_buy)} names for about "
               f"{money(buy_cost, dp=0)} at current prices.")
        note = ("How it's counted: each closed winning option position adds its profit to a pot for that underlying on "
                "the day it closed; whenever the pot covers a share, whole shares are bought at that day's close and "
                "the rest stays in the pot (<b>Cash left</b>). Losing positions are ignored. <b>(L)</b> columns count "
                "losses too: they come out of the same pot, so they have to be earned back before more shares are "
                "bought (shares already bought are kept; negative cash = losses still to recover). "
                "<b>Vs. plan</b> compares with all the shares you hold now, however you bought them.")
        if unpriced:
            note += f" {unpriced} closing day(s) had no price history, so no shares were bought then."
        return ('<figure class="card wide"><h3>Option profits reinvested in the underlying</h3>'
                f'<p class="sub">{sub}</p>'
                + self.table(show, {"Profits from wins": lambda v: money(v), "Shares": lambda v: f"{v:g}",
                                    "Avg cost": lambda v: money(v), "Cash left": lambda v: money(v),
                                    "Price now": lambda v: money(v), "Value now": lambda v: money(v),
                                    "Gain on shares": lambda v: money(v, True), "You hold": lambda v: f"{v:g}",
                                    "Vs. plan": diff, "Shares (L)": lambda v: f"{v:g}",
                                    "Cash left (L)": lambda v: money(v, True)},
                             tid="t_reinvest", pnl_cols=("Gain on shares", "Cash left (L)"))
                + f'<p class="note">{note}</p></figure>')


    def _open_options(self) -> str:
        a = self.a
        op = a.open_positions
        if op.empty or not (op["asset"] == "option").any():
            return ""
        today = (a.equity.index[-1] if a.equity is not None else date.today())
        oo = op[(op["asset"] == "option") & (pd.to_datetime(op["expiry"]).dt.date >= today)]
        out = []
        greek_rows = []
        market = self.market()
        for i, (und, g) in enumerate(oo.groupby("underlying")):
            mu = market.underlyings.get(und) if market else None
            spot = (mu.spot if mu and mu.spot else None) or _spot_from(a, und, g)
            for _, r in g.iterrows():
                snap = a.bundle.option_snapshots.get(r["symbol"], {})
                iv = snap.get("impliedVolatility")
                gk = snap.get("greeks") or {}
                if gk:
                    greek_rows.append({"Underlying": und, "Contract": _leg(r["symbol"]), "Qty": r["qty"],
                                       "IV": iv, "Δ shares": gk.get("delta", 0) * 100 * r["qty"],
                                       "Γ": gk.get("gamma", 0) * 100 * r["qty"],
                                       "Θ $/day": gk.get("theta", 0) * 100 * r["qty"],
                                       "Vega $/vol-pt": gk.get("vega", 0) * 100 * r["qty"]})
            sh = op[(op["asset"] == "stock") & (op["symbol"] == und)]
            shares = float(sh["qty"].sum()) if len(sh) else 0.0
            share_cost = float(sh["cost_basis"].sum()) if len(sh) else 0.0
            share_value = float(sh["market_value"].fillna(0).sum()) if len(sh) else 0.0
            if shares:
                greek_rows.append({"Underlying": und, "Contract": "shares", "Qty": shares, "IV": None,
                                   "Δ shares": shares, "Γ": 0.0, "Θ $/day": 0.0, "Vega $/vol-pt": 0.0})
            if not spot:
                continue
            out.append(self._readings_card(i, und, g, spot, today, shares, share_cost, share_value))
        html_g = ""
        if greek_rows:
            gdf = pd.DataFrame(greek_rows)
            tot = gdf.groupby("Underlying")[["Δ shares", "Γ", "Θ $/day", "Vega $/vol-pt"]].sum().reset_index()
            tot.insert(1, "Contract", "TOTAL")
            gdf = pd.concat([gdf, tot.assign(Qty=math.nan, IV=None)], ignore_index=True).sort_values(
                ["Underlying", "Contract"], key=lambda s: s.map(lambda v: (v == "TOTAL", v)))
            html_g = ('<figure class="card wide"><h3>Open option Greeks</h3><p class="sub">Position Greeks from Alpaca '
                      'snapshots. Δ in share-equivalents; Θ is dollars per day; vega is dollars per 1 vol point.</p>'
                      + self.table(gdf, {"Qty": lambda v: "" if pd.isna(v) else f"{v:g}", "IV": lambda v: "" if v is None or pd.isna(v) else pct(v, dp=0),
                                         "Δ shares": lambda v: num(v, 1), "Γ": lambda v: num(v, 2),
                                         "Θ $/day": lambda v: money(v, True), "Vega $/vol-pt": lambda v: money(v, True)},
                                   tid="t_greeks", pnl_cols=("Θ $/day",)) + '</figure>')
        card = self._rp_card(set(oo["underlying"]), "Edit a price or apply an IV change and the estimated value "
                             "and the dotted curve of every reading for that underlying update, here and on the "
                             "Orders tab.") if out else ""
        return card + "".join(out) + html_g

    def _readings_card(self, i, und, g, spot, today, shares, share_cost, share_value) -> str:
        """Every way to split this underlying's open legs into recognised strategies."""
        a = self.a
        rows = [{"symbol": r["symbol"], "qty": float(r["qty"]), "cost_basis": float(r["cost_basis"]),
                 "market_value": float(r["market_value"]) if pd.notna(r["market_value"]) else 0.0}
                for _, r in g.iterrows()]
        units, odd, split = build_units(rows, shares, share_cost, share_value, a.bundle.option_snapshots)
        # as-opened grouping from the ledger: open lots that share a strategy id
        by_combo: dict = {}
        for sym in g["symbol"]:
            for lot in a.ledger.open_lots.get(sym, []):
                n = int(round(abs(lot.qty))) if split else 1
                by_combo.setdefault(lot.combo_id or f"solo-{sym}", []).extend([sym] * max(n, 1))
        rs, whole = readings(units, spot, today, list(by_combo.values()))
        if not rs:
            return ""
        n_legs = len(g)
        market = self.market()
        model = None
        if market is not None and und in market.underlyings:
            snaps = a.bundle.option_snapshots
            models = O._models_for(units, snaps, market, und, RP.underlying_ivs(snaps))
            whole["today"] = O.curve_open(units, models, whole["x"])
            model = O.model_payload(units, models)
            est_unreal = float(np.interp(spot, whole["x"], whole["today"]))
        lot_shares = shares - (odd.qty if odd else 0.0)
        stock_note = f" + {lot_shares:g} shares" if lot_shares else ""
        odd_note = (f" {abs(odd.qty):g} odd-lot share{'s' if abs(odd.qty) != 1 else ''} (not a multiple of 100) "
                    f"{'are' if abs(odd.qty) != 1 else 'is'} left out: shares only count toward a strategy in "
                    f"100-share lots, one per contract." if odd else "")
        head = (f"Whole position: max gain {_lim(whole['max_gain'])} · max loss {_lim(whole['max_loss'])} · "
                f"breakevens {', '.join(f'${b:,.2f}' for b in whole['breakevens']) or 'none'} · unrealized "
                f"{money(whole['unrealized'], True)}" +
                (f" · Δ {num(whole['greeks'].get('delta', 0), 1)} sh · Θ {money(whole['greeks'].get('theta', 0), True)}/day"
                 if whole["greeks"].get("theta") else ""))
        if model:
            head += (f'<br><span class="rp-est" id="est_rd_{i}">Estimated at {_when(market)}: unrealized '
                     f'<b>{money(est_unreal, True)}</b> · value {money(est_unreal + model["cost"], True)}</span>')
        opts, views = [], []
        for j, r in enumerate(rs):
            vid = f"iv_{i}_{j}"
            tag = "As opened: " if r.as_opened else ""
            warn = f" ({r.undefined} undefined-risk)" if r.undefined else ""
            opts.append(f'<option value="{vid}">{html.escape(tag + r.label + warn)}</option>')
            data = []
            cols = [S1, S2, S3, S4, S5, S6, S7, S8]
            for k, grp in enumerate(r.groups):
                data.append({"type": "scatter", "mode": "lines", "name": f"{grp.strategy} ({grp.legs_label})",
                             "x": grp.curve_x, "y": grp.curve_expiry, "line": {"color": cols[k % 8], "width": 2}})
            data.append({"type": "scatter", "mode": "lines", "name": "Whole position at expiration", "x": whole["x"],
                         "y": whole["expiry"], "line": {"color": "var:ink", "width": 2, "dash": "dash"}})
            data.append({"type": "scatter", "mode": "lines",
                         "name": ("Whole position now (est.)" if market.is_open else "Whole position at next open (est.)")
                         if model else "Whole position today", "x": whole["x"],
                         "y": whole["today"], "line": {"color": NEUTRAL, "width": 2, "dash": "dot"}})
            if model:
                data.append({"type": "scatter", "mode": "markers", "name": "Estimated P&L at that price",
                             "x": [spot], "y": [round(est_unreal, 2)],
                             "marker": {"color": "var:ink", "size": 9, "symbol": "circle-open", "line": {"width": 2}},
                             "hovertemplate": "est. P&L %{y:$,.2f}<extra></extra>"})
                self.rp_views.append({"kind": "pos", "fid": f"f_read_{i}_{j}", "und": und, "x": whole["x"],
                                      "order": model, "pos": None, "net": 0.0, "um": None, "t_open": len(data) - 2,
                                      "t_mark": len(data) - 1, "t_comb": None, "est": f"est_rd_{i}", "rows": []})
            shapes = [{"type": "line", "x0": spot, "x1": spot, "yref": "paper", "y0": 0, "y1": 1,
                       "line": {"color": "var:muted", "width": 1, "dash": "dot"}},
                      {"type": "line", "xref": "paper", "x0": 0, "x1": 1, "y0": 0, "y1": 0,
                       "line": {"color": "var:axis", "width": 1}}]
            fig = self.fig(f"f_read_{i}_{j}", data, {
                "shapes": shapes, "hovermode": "x unified", "showlegend": True,
                "legend": {"orientation": "h", "y": -0.22, "yanchor": "top"},
                "xaxis": {"tickprefix": "$", "title": {"text": f"{und} price at expiration"}},
                "yaxis": {"tickprefix": "$"},
                "annotations": [{"x": spot, "yref": "paper", "y": 1, "text": f"spot ${spot:,.2f}", "showarrow": False,
                                 "yanchor": "bottom", "font": {"size": 11}}]}, 380)
            tbl = pd.DataFrame([{
                "Strategy": grp.strategy, "Legs": grp.legs_label, "Cost": grp.cost, "Value": grp.value,
                "Unrealized": grp.unrealized, "Max gain": grp.max_gain, "Max loss": grp.max_loss,
                "Breakevens": ", ".join(f"${b:,.2f}" for b in grp.breakevens) or "–",
                "Δ sh": grp.greeks.get("delta", math.nan), "Θ $/day": grp.greeks.get("theta", math.nan)}
                for grp in r.groups])
            sum_loss = r.sum_max_loss
            n_strat = sum(1 for grp in r.groups if grp.strategy != "Odd-lot shares")
            summary = (f"<p class=\"sub\">{n_strat} strateg{'ies' if n_strat != 1 else 'y'} · sum of each one's own max "
                       f"loss {_lim(sum_loss)}"
                       + (" (margin-style worst case: the groups' worst prices differ, so this overstates what the "
                          "legs can lose together, but it's what you could be left exposed to after closing some "
                          "groups and keeping others)" if n_strat > 1 and not math.isinf(sum_loss) else "")
                       + ".</p>")
            views.append(f'<div class="interp-view" id="{vid}"{"" if j == 0 else " hidden"}>{summary}'
                         + self.table(tbl, {"Cost": lambda v: money(v, True), "Value": lambda v: money(v, True),
                                            "Unrealized": lambda v: money(v, True), "Max gain": _lim, "Max loss": _lim,
                                            "Δ sh": lambda v: num(v, 1), "Θ $/day": lambda v: money(v, True)},
                                      tid=f"t_read_{i}_{j}", pnl_cols=("Unrealized",)) + fig + "</div>")
        more = "" if split else (" Legs with several contracts are kept whole here (too many contracts to split "
                                 "every way).")
        return (f'<figure class="card wide"><h3>{html.escape(und)}: {len(rs)} way{"s" if len(rs) != 1 else ""} to read '
                f'{n_legs} open leg{"s" if n_legs != 1 else ""}{stock_note}</h3>'
                f'<p class="sub">{head}</p>'
                f'<label class="pick">Interpretation <select class="interp" data-box="ib_{i}">{"".join(opts)}</select></label>'
                f'<div id="ib_{i}">{"".join(views)}</div>'
                f'<p class="note">Every reading is the same legs, so total P&L is identical. What changes is how '
                f'risk is grouped: per-group max loss and breakevens, and what you\'d be left holding if you closed '
                f'one group. Expiration curves value all legs at intrinsic, ignoring time value left on later-dated '
                f'legs of calendars/diagonals.{more}{odd_note}</p></figure>')

    # --------------------------------------------------------------- orders
    def _orders(self):
        a = self.a
        allo = O.parse_orders(a.bundle.orders)
        if not allo:
            return
        today = datetime.now(NY).date()
        spot = lambda und: _spot_from(a, und, None)  # noqa: E731
        snaps = a.bundle.option_snapshots
        market = self.market()
        O.evaluate(allo, a.ledger, a.open_positions, snaps, spot, today, market=market)
        opening = [o for o in allo if o.asset == "option" and o.effect != "close"]
        skipped_close = sum(o.asset == "option" and o.effect == "close" for o in allo)
        skipped_stock = sum(o.asset == "stock" for o in allo)
        skipped = []
        if skipped_close:
            skipped.append(f"{skipped_close} closing option order{'s' if skipped_close != 1 else ''}")
        if skipped_stock:
            skipped.append(f"{skipped_stock} stock order{'s' if skipped_stock != 1 else ''}")
        skip_note = (f'<p class="note">Not projected here: {" and ".join(skipped)} (take-profits and exits on '
                     f'positions you already hold).</p>') if skipped else ""
        if not opening:
            self.section("orders", "Orders", '<p class="empty">No open option strategy orders.</p>' + skip_note)
            return
        opening.sort(key=lambda o: (o.expiry or date.max, o.underlying, (o.submitted.timestamp() if o.submitted else 0)))
        priced = [o for o in opening if o.curves]
        when = _when(market)

        def px(v, est=False):
            if v is None or (isinstance(v, float) and math.isnan(v)):
                return "–"
            return f"${abs(v):,.2f} {'debit' if v > 0 else 'credit' if v < 0 else 'even'}" + (" (mkt est.)" if est else "")

        def rr(o):
            if math.isinf(o.max_gain) or math.isinf(o.max_loss) or not o.max_loss:
                return math.nan
            return o.max_gain / -o.max_loss

        def name(o):
            return f"{o.underlying} {o.strategy} {o.expiry:%m/%d}" if o.expiry else f"{o.underlying} {o.strategy}"

        def edge_est(o):
            if o.est_open is None or o.net is None or o.net_is_estimate:
                return math.nan
            return (o.est_open - o.net) * o.units * o.mult

        def fill_text(est_unit, net):
            if est_unit is None or net is None:
                return ""
            e = est_unit - net
            if e <= 0.005:
                return "at or through your limit: likely to fill near the open"
            return f"your limit is ${e:,.2f} better than the estimate: fills only if prices move your way"

        # ---- tiles
        net = sum(o.net * o.units * o.mult for o in opening if o.net is not None)
        fin_loss = [o.max_loss for o in priced if not math.isinf(o.max_loss)]
        fin_gain = [o.max_gain for o in priced if not math.isinf(o.max_gain)]
        n_unl = sum(math.isinf(o.max_loss) for o in priced)
        pops = [o.pop for o in priced if o.pop is not None]
        edge = [o.mid_now - o.net for o in opening if o.mid_now is not None and o.net is not None and not o.net_is_estimate]
        ests = [edge_est(o) for o in opening if not math.isnan(edge_est(o))]
        nearest = min((o.expiry for o in opening if o.expiry), default=None)
        stale = [u for u in market.underlyings.values() if u.stale and u.symbol in {o.underlying for o in opening}]
        tiles = [
            ("Open strategy orders", str(len(opening)),
             f"{len({o.underlying for o in opening})} underlyings"
             + (f" · first expiry {nearest:%b %d} ({(nearest - today).days} d)" if nearest else "")),
            ("If all fill", px(round(net)).replace(".00", ""), "net premium at your limit prices, before fees"),
            ("Combined max loss", money(sum(fin_loss), dp=0) if fin_loss else "–",
             (f"+ {n_unl} undefined-risk" if n_unl else "every order is defined-risk") + " · each order's own worst case"),
            ("Combined max profit", money(sum(fin_gain), dp=0) if fin_gain else "–",
             f"reward/risk {sum(fin_gain) / -sum(fin_loss):.2f}" if fin_loss and sum(fin_loss) else ""),
            ("Median prob. of profit", pct(float(np.median(pops)), dp=0) if pops else "–",
             "at first expiry, from implied vol"),
            ("Limits vs mid", f"{sum(e >= -0.005 for e in edge)}/{len(edge)}" if edge else "–",
             "priced at or better than the quoted mid (less likely to fill)"),
            ("Likely to fill", f"{sum(e <= 0.5 for e in ests)}/{len(ests)}" if ests else "–",
             f"limit at or through the estimated value at {'current prices' if market.is_open else 'the open'}"),
        ]
        if stale:
            tiles.insert(1, ("Moved since quotes", str(len(stale)),
                             ", ".join(f"{u.symbol} {u.move:+.1%}" for u in sorted(stale, key=lambda u: -abs(u.move)))))
        parts = [self.tiles(tiles)]

        # ---- repricing inputs
        parts.append(self._rp_card({o.underlying for o in priced},
                                   "Edit a price or apply an IV change (e.g. −30 for an earnings vol crush) and every "
                                   "estimate, table cell and chart for that underlying updates, here and on the "
                                   "Options tab."))

        # ---- risk / reward overview
        if priced:
            ys = [f"{name(o)} · #{k + 1}" for k, o in enumerate(priced)]
            gain = [None if math.isinf(o.max_gain) else round(o.max_gain, 2) for o in priced]
            loss = [None if math.isinf(o.max_loss) else round(o.max_loss, 2) for o in priced]
            cd = [[o.legs_label, px(o.net, o.net_is_estimate), "–" if o.pop is None else f"{o.pop:.0%}",
                   _lim(o.max_gain), _lim(o.max_loss)] for o in priced]
            ht = ("<b>%{y}</b><br>%{customdata[0]}<br>limit %{customdata[1]}<br>max profit %{customdata[3]} · "
                  "max loss %{customdata[4]}<br>prob. of profit %{customdata[2]}<extra></extra>")
            data = [{"type": "bar", "orientation": "h", "y": ys, "x": gain, "name": "Max profit",
                     "marker": {"color": GAIN}, "customdata": cd, "hovertemplate": ht},
                    {"type": "bar", "orientation": "h", "y": ys, "x": loss, "name": "Max loss",
                     "marker": {"color": LOSS}, "customdata": cd, "hovertemplate": ht}]
            if pops:
                data.append({"type": "scatter", "mode": "markers", "y": ys, "xaxis": "x2", "name": "Prob. of profit",
                             "x": [o.pop for o in priced], "marker": {"color": "var:ink", "symbol": "diamond", "size": 9},
                             "hovertemplate": "%{y}<br>prob. of profit %{x:.0%}<extra></extra>"})
            parts.append(self.fig("f_ord_rr", data, {
                "barmode": "relative", "margin": {"l": 230, "t": 36}, "yaxis": {"autorange": "reversed"},
                "xaxis": {"tickprefix": "$", "title": {"text": "max loss ← at expiration → max profit"}},
                "xaxis2": {"overlaying": "x", "side": "top", "range": [0, 1], "tickformat": ".0%",
                           "showgrid": False, "title": {"text": "probability of profit"}},
                "legend": {"orientation": "h", "y": -0.12, "yanchor": "top"}, "showlegend": True},
                max(280, 34 * len(priced) + 120), wide=True, title="Risk and reward if each order fills",
                subtitle="At your limit price, held to expiration, fees excluded. Undefined-risk sides are left "
                         "off the bars (see the table). Diamonds: chance the underlying finishes beyond breakeven, "
                         "from the legs' implied vol (lognormal, no drift, from the latest price) — a market-implied "
                         "estimate, not a forecast."))

        # ---- table
        rows = []
        for k, o in enumerate(opening):
            rows.append({"#": k + 1, "Underlying": o.underlying, "Strategy": o.strategy, "Legs": o.legs_label,
                         "Expires": o.expiry.isoformat() if o.expiry else "–",
                         "DTE": (o.expiry - today).days if o.expiry else math.nan,
                         "Limit": px(o.net, o.net_is_estimate), "Mid (quoted)": px(o.mid_now), "Natural": px(o.natural),
                         "Edge vs mid": (o.mid_now - o.net) * o.units * o.mult
                         if o.mid_now is not None and o.net is not None and not o.net_is_estimate else math.nan,
                         "Est. at open" if not market.is_open else "Est. now": px(o.est_open),
                         "Edge vs est.": edge_est(o),
                         "Max profit": o.max_gain, "Max loss": o.max_loss, "Reward/risk": rr(o),
                         "Breakevens": ", ".join(f"${b:,.2f}" for b in o.breakevens) or "–",
                         "Prob. profit": math.nan if o.pop is None else o.pop,
                         "Spot": o.spot if o.spot else math.nan, "TIF": o.tif.upper(),
                         "Placed": _iso(o.submitted) if o.submitted else "–"})
        parts.append('<figure class="card wide"><h3>Open option strategy orders</h3><p class="sub">Net price per '
                     'spread: debit you pay or credit you receive. Mid/natural are the last quotes (stale while the '
                     f'market is closed); <b>Est.</b> is the model value at {when} from the re-pricing inputs above. '
                     '<b>Edge</b> = dollars your limit is better than the mid / estimate (positive: a better price, but '
                     'less likely to fill; negative: you are paying up).</p>'
                     + self.table(pd.DataFrame(rows), {
                         "DTE": lambda v: num(v, 0), "Edge vs mid": lambda v: money(v, True),
                         "Edge vs est.": lambda v: money(v, True), "Max profit": _lim,
                         "Max loss": _lim, "Reward/risk": lambda v: num(v, 2),
                         "Prob. profit": lambda v: pct(v, dp=0), "Spot": lambda v: money(v)},
                         tid="t_orders", pnl_cols=("Edge vs mid", "Edge vs est.")) + '</figure>')

        # ---- payoff explorer
        opts, views = [], []
        rp_views = self.rp_views
        groups: dict[str, list] = {}
        for o in priced:
            groups.setdefault(o.underlying, []).append(o)
        k_of = {id(o): k + 1 for k, o in enumerate(opening)}
        items = []
        for und in sorted(groups, key=lambda u: min(k_of[id(o)] for o in groups[u])):   # same order as the # column
            os_ = groups[und]
            if len(os_) > 1:
                items.append(("all", und, os_))
            items += [("one", und, [o]) for o in os_]
        group_open = None
        for j, (kind, und, os_) in enumerate(items):
            vid = f"ov_{j}"
            if kind == "all":
                d = O.combine(os_, a.open_positions, snaps, today, market=market)
                if not d:
                    continue
                label = f"{und}: all {len(os_)} orders together"
                cur = {"x": d["x"], "expiry": d["expiry"], "today": d["today"]}
                for key in ("position", "combined", "combined_today"):
                    if key in d:
                        cur[key] = d[key]
                stats = (f"{len(os_)} orders · net {px(d['net'])} · max profit {_lim(d['max_gain'])} · max loss "
                         f"{_lim(d['max_loss'])} · breakevens {', '.join(f'${b:,.2f}' for b in d['breakevens']) or 'none'}")
                comb = d
                sp = d["spot"]
                exp_lbl = min(o.expiry for o in os_ if o.expiry)
                model, net_total, um, rows_k = d.get("model"), d["net"], None, []
                est_total = d.get("est_open")
            else:
                o = os_[0]
                label = f"#{k_of[id(o)]} {name(o)} @ {px(o.net, o.net_is_estimate)}"
                cur = o.curves
                stats = (f"{o.legs_label} · limit {px(o.net, o.net_is_estimate)} (quoted mid {px(o.mid_now)}, natural "
                         f"{px(o.natural)}) · max profit {_lim(o.max_gain)} · max loss {_lim(o.max_loss)} · reward/risk "
                         f"{num(rr(o), 2)} · breakevens {', '.join(f'${b:,.2f}' for b in o.breakevens) or 'none'}"
                         + (f" · prob. of profit ~{o.pop:.0%}" if o.pop is not None else ""))
                comb = {k2.replace("comb_", ""): v for k2, v in o.curves.items() if k2.startswith("comb_")}
                sp = o.spot
                exp_lbl = o.expiry
                model, um, rows_k = o.model or None, o.units * o.mult, [k_of[id(o)]]
                net_total = o.net * um
                est_total = o.est_open * um if o.est_open is not None else None
            if model:
                eu = None if um is None or est_total is None else est_total / um
                est_txt = (f"Estimated value at {when}: <b>{px(est_total if um is None else eu)}</b>"
                           + (f" vs your limit {px(net_total / um)} · {fill_text(eu, net_total / um)}" if um else
                              f" vs {px(net_total)} at your limits"))
                stats += f'<br><span class="rp-est" id="est_{vid}">{est_txt}</span>'
            if "combined" in cur:
                stats += (f"<br>With your current {html.escape(und)} position: max loss after fill "
                          f"{_lim(comb.get('max_loss'))} · max profit {_lim(comb.get('max_gain'))} · breakevens "
                          f"{', '.join(f'${b:,.2f}' for b in comb.get('breakevens') or []) or 'none'}")
            open_name = ("Order now (est.)" if market.is_open else "Order at next open (est.)") if model else "Order today (at IV)"
            data = [{"type": "scatter", "mode": "lines", "name": "Order at expiration", "x": cur["x"],
                     "y": cur["expiry"], "line": {"color": S1, "width": 2.5}},
                    {"type": "scatter", "mode": "lines", "name": open_name, "x": cur["x"],
                     "y": cur["today"], "line": {"color": S1, "width": 1.5, "dash": "dot"}}]
            t_mark = t_comb = None
            if model:
                y0 = float(np.interp(sp, cur["x"], cur["today"]))
                t_mark = len(data)
                data.append({"type": "scatter", "mode": "markers", "name": "Estimated P&L at that price",
                             "x": [sp], "y": [round(y0, 2)], "marker": {"color": S1, "size": 9, "symbol": "circle-open",
                                                                         "line": {"width": 2}},
                             "hovertemplate": "est. P&L %{y:$,.2f}<extra></extra>"})
            if "combined" in cur:
                data += [{"type": "scatter", "mode": "lines", "name": "Current position at expiration",
                          "x": cur["x"], "y": cur["position"], "line": {"color": NEUTRAL, "width": 1.5}},
                         {"type": "scatter", "mode": "lines", "name": "Position + order at expiration",
                          "x": cur["x"], "y": cur["combined"], "line": {"color": "var:ink", "width": 2, "dash": "dash"}}]
                if model and model.get("pos") and cur.get("combined_today"):
                    t_comb = len(data)
                    data.append({"type": "scatter", "mode": "lines", "name": "Position + order at next open (est.)",
                                 "x": cur["x"], "y": cur["combined_today"],
                                 "line": {"color": "var:ink", "width": 1.2, "dash": "dot"}})
            shapes = [{"type": "line", "x0": sp, "x1": sp, "yref": "paper", "y0": 0, "y1": 1,
                       "line": {"color": "var:muted", "width": 1, "dash": "dot"}},
                      {"type": "line", "xref": "paper", "x0": 0, "x1": 1, "y0": 0, "y1": 0,
                       "line": {"color": "var:axis", "width": 1}}]
            mu = market.underlyings.get(und)
            spot_lbl = ("latest" if mu and mu.stale else "spot")
            fig = self.fig(f"f_ord_{j}", data, {
                "shapes": shapes, "hovermode": "x unified", "showlegend": True,
                "legend": {"orientation": "h", "y": -0.22, "yanchor": "top"},
                "xaxis": {"tickprefix": "$", "title": {"text": f"{und} price at expiration"
                                                               + (f" ({exp_lbl:%b %d})" if exp_lbl else "")}},
                "yaxis": {"tickprefix": "$"},
                "annotations": [{"x": sp, "yref": "paper", "y": 1, "text": f"{spot_lbl} ${sp:,.2f}", "showarrow": False,
                                 "yanchor": "bottom", "font": {"size": 11}}]}, 380)
            if model:
                rp_views.append({"kind": "order", "fid": f"f_ord_{j}", "und": und, "x": cur["x"], "order": model["order"],
                                 "pos": model.get("pos") if t_comb is not None else None, "net": net_total, "um": um,
                                 "t_open": 1, "t_mark": t_mark, "t_comb": t_comb, "est": f"est_{vid}", "rows": rows_k})
            if und != group_open:
                opts.append(("</optgroup>" if group_open else "") + f'<optgroup label="{html.escape(und)}">')
                group_open = und
            opts.append(f'<option value="{vid}">{html.escape(label)}</option>')
            views.append(f'<div class="interp-view" id="{vid}"{"" if not views else " hidden"}>'
                         f'<p class="sub">{stats}</p>{fig}</div>')
        if group_open:
            opts.append("</optgroup>")
        if views:
            parts.append(f'<figure class="card wide"><h3>Payoff if filled</h3>'
                         f'<label class="pick">Order <select class="interp" data-box="ob">{"".join(opts)}</select></label>'
                         f'<div id="ob">{"".join(views)}</div>'
                         f'<p class="note">P&amp;L at your limit price, fees excluded. Expiration curves value every leg at '
                         f'intrinsic (later-dated legs of calendars/diagonals are approximated). The dotted curve is the '
                         f'estimated value at {when} from the re-pricing inputs. Where you already hold the underlying '
                         f'or options on it, the dashed line shows the whole position after the fill.</p></figure>')
        parts.append(skip_note)
        self.section("orders", "Orders", "".join(parts))

    def _rp_card(self, unds, how: str) -> str:
        """Per-underlying re-pricing inputs. Rows for the same underlying stay in sync across tabs."""
        market = self.market()
        if market is None:
            return ""
        rows_html = []
        for und in sorted(unds, key=lambda u: -abs(market.underlyings[u].move) if u in market.underlyings else 0):
            u = market.underlyings.get(und)
            if not u or not u.spot:
                continue
            lt = (f"${u.last:,.2f} · {u.last_size:,.0f} sh · {u.last_time.astimezone(NY):%a} "
                  f"{_clock(u.last_time.astimezone(NY))}"
                  + (f" ({u.last_feed})" if u.last_feed not in ("", "primary") else "")
                  + (" · after hours" if u.last_ext else "")) if u.last and u.last_time else "–"
            qt = f"{u.quote_time.astimezone(NY):%a} {_clock(u.quote_time.astimezone(NY))}" if u.quote_time else "–"
            rows_html.append(
                f'<tr class="{"stale" if u.stale else ""}" data-und="{html.escape(und)}">'
                f'<td><b>{html.escape(und)}</b></td><td>{money(u.ref)}</td><td>{lt}</td>'
                f'<td class="rp-move">{pct(u.move, True)}</td><td>{qt}</td>'
                f'<td><input class="rp-spot" type="number" inputmode="decimal" min="0" step="any" '
                f'value="{round(u.spot, 4):g}" data-default="{round(u.spot, 4):g}" data-ref="{u.ref or u.spot:.6f}" '
                f'aria-label="{html.escape(und)} price for estimates"></td>'
                f'<td><input class="rp-iv" type="number" inputmode="decimal" step="5" min="-90" max="300" value="0" '
                f'aria-label="{html.escape(und)} IV change in percent"> %</td>'
                f'<td><button type="button" class="rp-reset">Reset</button></td></tr>')
        if not rows_html:
            return ""
        when = _when(market)
        status = ("The market is open: option quotes are live, so the inputs are what-ifs." if market.is_open else
                  f"The market is closed. Option quotes are from the last session; the underlying keeps trading "
                  f"after hours and overnight. Estimates are for {when}.")
        return ('<figure class="card wide"><h3>Re-pricing: underlying moves since the option quotes</h3>'
                f'<p class="sub">{status} Rows with a move of {RP.STALE_MOVE:.0%} or more are highlighted. {how}</p>'
                '<div class="tablewrap"><table class="rp-table"><thead><tr>'
                + "".join(f'<th{_tip_attr(h)}>{h}</th>' for h in ("Underlying", "Quotes reflect", "Latest trade", "Move",
                                                                  "Options quoted", "Price for estimates", "IV change", ""))
                + '</tr></thead><tbody>' + "".join(rows_html) + '</tbody></table></div>'
                '<p class="note">How the estimate works: each leg\'s implied vol is backed out of its own quoted mid at '
                'the price the quotes reflect (one-sided or very wide quotes fall back to Alpaca\'s IV), then the leg is '
                f're-priced with Black-Scholes at the new price and the time left at {when}, counted in trading days '
                '(weekends and market holidays don\'t decay). Each strike keeps its own IV (sticky strike) unless you '
                'change it. Single after-hours prints can be far from where the stock opens: check the size and time. '
                'European pricing, r = 4%, no dividends.</p></figure>')

    # ------------------------------------------------------------ positions
    def _positions(self):
        a = self.a
        op = a.open_positions
        if op.empty:
            return
        parts = []
        p = op.dropna(subset=["unrealized_pl"]).sort_values("unrealized_pl")
        data = [{"type": "bar", "orientation": "h", "y": p["symbol"].map(_leg).tolist(), "x": p["unrealized_pl"].round(2).tolist(),
                 "marker": {"color": [_pn(v) for v in p["unrealized_pl"]]}, "showlegend": False,
                 "customdata": np.c_[p["market_value"], p["unrealized_pct"].fillna(0)].round(4).tolist(),
                 "hovertemplate": "%{y}<br>%{x:$,.2f} (%{customdata[1]:+.1%})<br>value %{customdata[0]:$,.2f}<extra></extra>"}]
        parts.append(self.fig("f_unreal", data, {"xaxis": {"tickprefix": "$"}, "margin": {"l": 150}},
                              max(300, 22 * len(p) + 60), title="Unrealized P&L by position",
                              subtitle=f"Total {money(p['unrealized_pl'].sum(), True)} on cost basis "
                                       f"{money(p['cost_basis'].sum())}; cost basis is fee-inclusive."))
        stocks = op[(op["asset"] == "stock") & op["market_value"].notna()].sort_values("market_value", ascending=False)
        if len(stocks):
            tot = stocks["market_value"].sum()
            data = [{"type": "bar", "x": stocks["symbol"].tolist(), "y": (stocks["market_value"] / tot).round(4).tolist(),
                     "marker": {"color": S1}, "showlegend": False,
                     "customdata": stocks["market_value"].round(2).tolist(),
                     "hovertemplate": "%{x}: %{y:.1%} · %{customdata:$,.2f}<extra></extra>"}]
            parts.append(self.fig("f_conc", data, {"yaxis": {"tickformat": ".0%"}, "bargap": 0.3}, 280,
                                  title="Stock concentration (share of long stock value)",
                                  subtitle=f"Top 3 = {pct((stocks['market_value'].head(3).sum()) / tot, dp=0)} of "
                                           f"{money(tot, dp=0)}"))
        show = op[["symbol", "asset", "qty", "avg_price", "broker_avg_price", "price", "market_value", "cost_basis",
                   "unrealized_pl", "unrealized_pct"]].copy()
        show.columns = ["Symbol", "Asset", "Qty", "Avg cost (eff.)", "Broker avg", "Price", "Value", "Cost basis",
                        "Unrealized", "%"]
        parts.append('<figure class="card wide"><h3>Open positions</h3><p class="sub">Avg cost = fee-inclusive cost '
                     'basis ÷ quantity from the ledger; “Broker avg” is Alpaca’s figure for comparison.</p>'
                     + self.table(show, {"Qty": lambda v: f"{v:g}", "Avg cost (eff.)": lambda v: num(v, 4),
                                         "Broker avg": lambda v: num(v, 4), "Price": lambda v: num(v, 4),
                                         "Value": money, "Cost basis": money, "Unrealized": lambda v: money(v, True),
                                         "%": lambda v: pct(v, True)}, tid="t_pos", pnl_cols=("Unrealized", "%"))
                     + '</figure>')
        self.section("positions", "Positions", "".join(parts))

    # ---------------------------------------------------------------- notes
    def _notes(self):
        a = self.a
        items = []
        rec = a.reconciliation
        if self.share:
            if rec is not None:
                ok = bool(rec["matched"].iloc[0])
                items.append("<li><b>Ledger vs. broker positions:</b> "
                             + ("rebuilt positions match the broker exactly." if ok else
                                "some rebuilt positions differ from the broker (history may be incomplete).") + "</li>")
            rec = None
        if rec is not None:
            items.append("<li><b>Ledger vs. broker positions:</b> "
                         + ("every open quantity matches Alpaca exactly." if rec.empty else
                            f"{len(rec)} mismatches – history may be incomplete:" + self.table(rec, tid="t_rec"))
                         + "</li>")
        cs = a.cash_summary
        fee_amt = "" if self.share else (f" {money(cs.get('fees', 0))} total; {money(cs.get('fees_in_trades', 0))} "
                                          "folded into fill prices")
        items.append(f"<li><b>Fees:</b>{fee_amt or ' folded into fill prices'} (order-linked fees to their order, daily regulatory fees to that day's fills: "
                     "TAF/REG→sells, ORF/OCC→option contracts, CAT→all executions).</li>")
        items.append("<li><b>Assignments & exercises</b> close the option at $0 and deliver stock at the strike, as "
                      "Alpaca books them: the option premium stays on the option, the stock P&L starts at the strike.</li>")
        items.append("<li><b>Returns</b> are time-weighted (Modified Dietz, flows at start of day) so deposits never "
                     "count as performance. Benchmarks are run with the same deposits on the same days.</li>")
        if a.start and not self.share:
            early = a.ledger.closed_df()
            early = early[early["close_time"].dt.date < a.start] if not early.empty else early
            if len(early):
                items.append(f"<li><b>Window:</b> analysis starts {a.start:%b %d, %Y}. {len(early)} earlier closed "
                             f"lots ({money(early['pnl'].sum(), True)}) are excluded; pass <code>--start</code> "
                             "to change the window.</li>")
        for w in a.warnings:
            items.append(f"<li>{html.escape(w)}</li>")
        self.section("notes", "Method & data checks", f'<div class="card wide prose"><ul>{"".join(items)}</ul></div>')


# --------------------------------------------------------------------- helpers
def _clean(o):
    """Recursively replace NaN/inf with None so the figure JSON is always valid."""
    if isinstance(o, dict):
        return {k: _clean(v) for k, v in o.items()}
    if isinstance(o, (list, tuple)):
        return [_clean(v) for v in o]
    if isinstance(o, (float, np.floating)):
        return None if math.isnan(o) or math.isinf(o) else float(o)
    return o


def _json_default(o):
    if isinstance(o, (np.integer,)):
        return int(o)
    if isinstance(o, (np.floating,)):
        return None if math.isnan(o) or math.isinf(o) else float(o)
    if isinstance(o, (date, datetime, pd.Timestamp)):
        return _iso(o)
    raise TypeError(type(o))


def _profit_factor(t: pd.DataFrame) -> float:
    if t.empty:
        return math.nan
    loss = -t.loc[t["pnl"] < 0, "pnl"].sum()
    return t.loc[t["pnl"] > 0, "pnl"].sum() / loss if loss > 0 else math.inf


def _streaks(pnl: pd.Series) -> tuple[int, int]:
    best_w = best_l = cw = cl = 0
    for v in pnl:
        if v > 0:
            cw, cl = cw + 1, 0
        elif v < 0:
            cl, cw = cl + 1, 0
        best_w, best_l = max(best_w, cw), max(best_l, cl)
    return best_w, best_l


def _dur(days: float) -> str:
    if days is None or (isinstance(days, float) and math.isnan(days)):
        return "–"
    if days < 1 / 24:
        return f"{days * 1440:.0f} min"
    if days < 1:
        return f"{days * 24:.1f} h"
    return f"{days:.1f} d"


def _nice_bins(s: pd.Series, target: int = 24):
    lo, hi = float(s.min()), float(s.max())
    span = max(hi - lo, 1.0)
    raw = span / target
    mag = 10 ** math.floor(math.log10(raw))
    step = min((m * mag for m in (1, 2, 2.5, 5, 10) if m * mag >= raw), default=raw)
    start = math.floor(lo / step) * step
    edges = np.arange(start, hi + step, step)
    # keep zero on an edge so wins and losses never share a bar
    return edges if len(edges) > 1 else np.array([lo - 1, hi + 1])


def _tip_attr(label: str) -> str:
    t = _tip(label)
    return f' data-tip="{html.escape(t)}" tabindex="0"' if t else ""


def _when(market) -> str:
    if market.is_open:
        return "now"
    v = market.valuation.astimezone(NY)
    return f"the next open ({v:%a %b} {v.day}, {_clock(v)} ET)"


def _clock(d) -> str:
    """'9:30 AM' - portable (Pyodide's strftime has no %-I)."""
    return f"{d.hour % 12 or 12}:{d.minute:02d} {'AM' if d.hour < 12 else 'PM'}"


def _lim(v) -> str:
    if v is None or (isinstance(v, float) and math.isnan(v)):
        return "–"
    if math.isinf(v):
        return "unlimited"
    return money(v, True)


def _leg(sym: str) -> str:
    c = parse_occ(sym)
    return f"{c.underlying} {c.expiry:%m/%d} {c.strike:g}{c.right}" if c else sym


def _spot_from(a: Analysis, und: str, g: pd.DataFrame) -> float | None:
    op = a.open_positions
    s = op[(op["symbol"] == und) & op["price"].notna()] if not op.empty else op
    if len(s):
        return float(s["price"].iloc[0])
    if und in a.bundle.prices:
        return float(a.bundle.prices[und])
    bars = a.bundle.bars.get(und)
    if bars:
        return float(bars[-1]["c"])
    return None


def render(a: Analysis, path: str | Path, inline_js: str | None = None) -> Path:
    return Report(a).render(path, inline_js)
