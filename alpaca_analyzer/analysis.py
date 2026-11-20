"""Assemble every table the report needs from a :class:`~alpaca_analyzer.bundle.Bundle`."""
from __future__ import annotations

import math
from dataclasses import dataclass, field
from datetime import date

import numpy as np
import pandas as pd

from . import metrics as M
from .bundle import Bundle
from .ledger import NY, Ledger, allocate_fees, build_ledger, max_risk, normalize
from .occ import parse_occ, underlying_of


@dataclass
class Analysis:
    bundle: Bundle
    ledger: Ledger
    trades: pd.DataFrame            # closed FIFO lots
    combos: pd.DataFrame            # option strategies
    open_positions: pd.DataFrame    # from Alpaca positions (marks) or ledger lots
    symbols: pd.DataFrame           # per-underlying scorecard incl. buy & hold comparison
    equity: pd.DataFrame | None = None
    bench: dict[str, pd.DataFrame] = field(default_factory=dict)
    stats: M.PerfStats | None = None
    bench_stats: dict[str, M.PerfStats] = field(default_factory=dict)
    drawdowns: pd.DataFrame | None = None
    rolling: pd.DataFrame | None = None
    monthly: pd.DataFrame | None = None
    cash_summary: dict = field(default_factory=dict)
    reconciliation: pd.DataFrame | None = None
    primary_benchmark: str = "SPY"
    warnings: list[str] = field(default_factory=list)
    alerts: list[str] = field(default_factory=list)   # data problems shown prominently in the report
    start: date | None = None


def run(bundle: Bundle, benchmarks: list[str] | None = None, combo_window_s: float = 120.0,
        start: str | None = None, rf: float = 0.0) -> Analysis:
    benchmarks = benchmarks or ["SPY", "QQQ"]
    events = normalize(bundle.activities)
    allocate_fees(events, _daily_fee_fallback(bundle))
    ledger = build_ledger(events, combo_window_s=combo_window_s)
    trades = ledger.closed_df()
    start_d = date.fromisoformat(start) if start else _equity_start(bundle, ledger)
    if start_d is not None and not trades.empty:
        trades = trades[trades["close_time"].dt.date >= start_d]

    marks = _marks(bundle)
    open_pos = _open_positions(bundle, ledger, marks)
    combos = _combos(ledger, trades, open_pos, bundle, start_d)
    a = Analysis(bundle, ledger, trades, combos, open_pos, pd.DataFrame(), warnings=list(ledger.warnings),
                 start=start_d)
    a.cash_summary = _cash_summary(ledger, start_d or _equity_start(bundle, ledger))
    if not a.cash_summary.get("fees"):
        fb = _daily_fee_fallback(bundle)
        if fb:  # no FEE activities supplied: totals come from portfolio history
            a.cash_summary["fees"] = float(sum(fb.values()))
    a.cash_summary["fees_in_trades"] = float(sum(e.fee for e in ledger.events if e.kind == "trade"))

    # ------------------------------------------------------ account-level series
    if bundle.portfolio_history:
        eq = M.equity_series(bundle.portfolio_history, ledger)
        a.alerts += _coverage_alerts(eq, ledger)
        eq = _append_live_point(eq, bundle, ledger)
        if start_d is not None:
            eq = eq[eq.index >= start_d].copy()
            eq.iloc[0, eq.columns.get_loc("flow")] = eq["equity"].iloc[0]
        a.equity = M.add_returns(eq)
        for sym in benchmarks:
            closes = M.bar_closes(bundle.bars, sym)
            if closes is not None and len(closes) > 2:
                a.bench[sym] = M.benchmark_frame(a.equity, closes)
            else:
                a.warnings.append(f"No price bars for benchmark {sym}; skipped.")
        a.primary_benchmark = next(iter(a.bench), benchmarks[0])
        pb = a.bench.get(a.primary_benchmark)
        a.stats = M.perf_stats(a.equity, pb, rf)
        for sym, bf in a.bench.items():
            bdf = bf.rename(columns={"value": "equity"}).assign(flow=a.equity["flow"])
            bdf["pnl"] = bdf["equity"].diff().fillna(0) - bdf["flow"].where(bdf.index != bdf.index[0], 0)
            bdf["net_deposits"] = a.equity["net_deposits"]
            bdf["dd_dollar"] = bdf["cum_pnl"] - bdf["cum_pnl"].cummax()
            a.bench_stats[sym] = M.perf_stats(bdf, None, rf)
        a.drawdowns = M.drawdown_periods(a.equity)
        a.rolling = M.rolling_stats(a.equity, pb)
        a.monthly = M.monthly_returns(a.equity)
    else:
        a.warnings.append("No portfolio history: account-level equity/drawdown charts are disabled.")

    a.symbols = _symbol_scorecard(a, marks)
    a.reconciliation = _reconcile(ledger, bundle)
    return a


# ---------------------------------------------------------------------- helpers
def _marks(bundle: Bundle) -> dict[str, float]:
    m: dict[str, float] = {}
    for sym in bundle.bars:
        c = M.bar_closes(bundle.bars, sym)
        if c is not None and len(c):
            m[sym] = float(c.iloc[-1])
    m.update({k: float(v) for k, v in bundle.prices.items()})
    for p in bundle.positions:
        if p.get("current_price") not in (None, ""):
            m[p["symbol"]] = float(p["current_price"])
    for sym, snap in bundle.option_snapshots.items():
        q = snap.get("latestQuote") or {}
        if q.get("bp") and q.get("ap"):
            m.setdefault(sym, (float(q["bp"]) + float(q["ap"])) / 2)
    return m


def _open_positions(bundle: Bundle, ledger: Ledger, marks: dict[str, float]) -> pd.DataFrame:
    rows = []
    if bundle.positions:
        for p in bundle.positions:
            sym = p["symbol"]
            c = parse_occ(sym)
            qty = float(p["qty"])
            mult = 100.0 if c else 1.0
            px = float(p["current_price"]) if p.get("current_price") not in (None, "") else math.nan
            mv = float(p["market_value"]) if p.get("market_value") not in (None, "") else math.nan
            broker_cost = float(p.get("cost_basis") or 0.0)
            lots = ledger.open_lots.get(sym, [])
            if lots and abs(sum(l.qty for l in lots) - qty) < 1e-6:
                cost = sum(l.qty * l.price for l in lots) * mult   # fee-inclusive, from qty x cost
            else:
                cost = broker_cost                                  # history incomplete for this symbol
            upl = mv - cost if not math.isnan(mv) else math.nan
            rows.append({"symbol": sym, "underlying": underlying_of(sym), "asset": "option" if c else "stock",
                         "qty": qty, "avg_price": cost / qty / mult if qty else math.nan, "price": px,
                         "broker_avg_price": float(p.get("avg_entry_price") or math.nan),
                         "market_value": mv, "cost_basis": cost, "unrealized_pl": upl,
                         "unrealized_pct": upl / abs(cost) if cost else math.nan,
                         "multiplier": mult, "expiry": c.expiry if c else None,
                         "right": c.right if c else None, "strike": c.strike if c else None})
    else:
        for sym, lots in ledger.open_lots.items():
            c = parse_occ(sym)
            mult = 100.0 if c else 1.0
            qty = sum(l.qty for l in lots)
            cost = sum(l.qty * l.price for l in lots) * mult
            px = marks.get(sym, math.nan)
            mv = qty * px * mult
            rows.append({"symbol": sym, "underlying": underlying_of(sym), "asset": "option" if c else "stock",
                         "qty": qty, "avg_price": cost / qty / mult if qty else 0, "price": px,
                         "market_value": mv, "cost_basis": cost, "unrealized_pl": mv - cost,
                         "unrealized_pct": (mv - cost) / abs(cost) if cost else math.nan, "multiplier": mult,
                         "expiry": c.expiry if c else None, "right": c.right if c else None,
                         "strike": c.strike if c else None})
    return pd.DataFrame(rows)


def _combos(ledger: Ledger, trades: pd.DataFrame, open_pos: pd.DataFrame, bundle: Bundle,
            start_d: date | None) -> pd.DataFrame:
    if not ledger.combos:
        return pd.DataFrame()
    t = ledger.closed_df()
    t = t[t["combo_id"].notna()]
    by = t.groupby("combo_id")
    realized = by["pnl"].sum()
    last_close = by["close_time"].max()
    kinds = by["close_kind"].agg(lambda s: set(s))
    # unrealized from still-open lots, marked to market
    unreal: dict[int, float] = {}
    open_qty: dict[int, float] = {}
    mark = {}
    if not open_pos.empty:
        mark = dict(zip(open_pos["symbol"], open_pos["price"]))
    for sym, lots in ledger.open_lots.items():
        for l in lots:
            if l.combo_id is None:
                continue
            px = mark.get(sym, math.nan)
            open_qty[l.combo_id] = open_qty.get(l.combo_id, 0) + abs(l.qty)
            if not math.isnan(px):
                unreal[l.combo_id] = unreal.get(l.combo_id, 0) + (px - l.price) * l.qty * l.multiplier
    closes = {}
    for cid, df in t.groupby("combo_id"):
        closes[cid] = df
    underlying_px = {s: M.bar_closes(bundle.bars, s) for s in {c.underlying for c in ledger.combos.values()}}

    rows = []
    for cid, c in ledger.combos.items():
        legs = {s: q for s, q in c.legs.items()}
        exps = [parse_occ(s).expiry for s in legs]
        open_t = pd.Timestamp(c.open_time).tz_convert(NY)
        is_open = open_qty.get(cid, 0) > 1e-9
        k = kinds.get(cid, set())
        if is_open:
            outcome = "Open"
        elif "assigned" in k:
            outcome = "Assigned"
        elif "exercised" in k:
            outcome = "Exercised"
        elif k == {"expired"}:
            outcome = "Expired"
        elif "expired" in k:
            outcome = "Closed / partly expired"
        else:
            outcome = "Closed early"
        mr = max_risk(c)
        pnl_r = float(realized.get(cid, 0.0))
        pnl_u = float(unreal.get(cid, 0.0))
        und_px = math.nan
        s = underlying_px.get(c.underlying)
        if s is not None and len(s):
            prior = s[s.index <= open_t.date()]
            if len(prior):
                und_px = float(prior.iloc[-1])
        moneyness = math.nan
        if len(legs) == 1 and not math.isnan(und_px):
            occ = parse_occ(next(iter(legs)))
            moneyness = (und_px / occ.strike - 1) if occ.is_call else (occ.strike / und_px - 1)
        close_t = last_close.get(cid) if not is_open else pd.NaT
        rows.append({
            "combo_id": cid, "underlying": c.underlying, "strategy": c.strategy, "open_time": open_t,
            "close_time": close_t, "n_legs": len(legs),
            "legs": ", ".join(f"{'+' if q > 0 else ''}{q:g} {_leg_label(s)}" for s, q in legs.items()),
            "net_premium": c.open_cash, "premium_side": "Credit" if c.open_cash > 0 else "Debit",
            "max_risk": mr, "dte": (min(exps) - open_t.date()).days,
            "realized_pnl": pnl_r, "unrealized_pnl": pnl_u, "pnl": pnl_r + pnl_u, "outcome": outcome,
            "is_open": is_open, "rolled_from": c.rolled_from, "rolled_to": c.rolled_to,
            "underlying_px": und_px, "moneyness": moneyness,
            "rights": "".join(sorted({parse_occ(s).right for s in legs})),
        })
    df = pd.DataFrame(rows)
    df["hold_days"] = (pd.to_datetime(df["close_time"]) - df["open_time"]).dt.total_seconds() / 86400
    df["ror"] = df["pnl"] / df["max_risk"].where(df["max_risk"] > 0)
    # campaigns: follow roll chains to their root
    root = {}
    for cid in df["combo_id"]:
        r, seen = cid, set()
        while ledger.combos[r].rolled_from and r not in seen:
            seen.add(r)
            r = ledger.combos[r].rolled_from
        root[cid] = r
    df["campaign"] = df["combo_id"].map(root)
    if start_d is not None:
        df = df[(df["is_open"]) | (pd.to_datetime(df["close_time"]).dt.date >= start_d)]
    return df.sort_values("open_time").reset_index(drop=True)


def _daily_fee_fallback(bundle: Bundle) -> dict[date, float]:
    """{trade date: fee total} from portfolio history; only used when no FEE activities exist."""
    if any((a.get("activity_type") or "").upper() == "FEE" for a in bundle.activities):
        return {}
    ph = bundle.portfolio_history or {}
    fees = (ph.get("cashflow") or {}).get("FEE")
    if not fees:
        return {}
    ts = pd.to_datetime(ph["timestamp"], unit="s", utc=True).tz_convert(NY)
    return {t.date(): float(v) for t, v in zip(ts, fees) if v}


def _equity_start(bundle: Bundle, ledger: Ledger) -> date | None:
    if not bundle.portfolio_history:
        return None
    try:
        return M.equity_series(bundle.portfolio_history, ledger).index[0]
    except ValueError:
        return None


def _leg_label(sym: str) -> str:
    c = parse_occ(sym)
    return f"{c.expiry:%m/%d} {c.strike:g}{c.right}" if c else sym


def _cash_summary(ledger: Ledger, start_d: date | None) -> dict:
    cash = ledger.cash_df()
    if cash.empty:
        return {}
    if start_d is not None:
        cash = cash[cash["date"] >= start_d]
    g = cash.groupby("kind")["cash"].sum()
    inc = cash[cash["kind"] == "income"].groupby("symbol")["cash"].sum()
    return {"fees": float(g.get("fee", 0.0)), "income": float(g.get("income", 0.0)),
            "interest": float(g.get("interest", 0.0)), "flows": float(g.get("flow", 0.0)),
            "income_by_symbol": inc.to_dict()}


def _append_live_point(eq: pd.DataFrame, bundle: Bundle, ledger: Ledger) -> pd.DataFrame:
    """Portfolio history ends at the previous close; add today's live equity from /account."""
    acct = bundle.account or {}
    if not acct.get("equity") or not ledger.events:
        return eq
    last_activity = max(e.ny_date for e in ledger.events if e.kind == "trade") if any(
        e.kind == "trade" for e in ledger.events) else None
    today = last_activity
    if today is None or today <= eq.index[-1] or today.weekday() >= 5:
        return eq
    if (today - eq.index[-1]).days > 5:
        return eq  # history doesn't reach today: a lone live point would draw a false straight line
    flows = sum(e.cash for e in ledger.events if e.kind == "flow" and e.ny_date == today)
    eq = eq.copy()
    eq.loc[today] = {"equity": float(acct["equity"]), "flow": flows}
    return eq


def _coverage_alerts(eq: pd.DataFrame, ledger: Ledger) -> list[str]:
    """Detect a portfolio history that doesn't cover the trading activity."""
    alerts = []
    trades = [e.ny_date for e in ledger.events if e.kind == "trade"]
    last_ph = eq.index[-1]
    if trades and (max(trades) - last_ph).days > 7:
        alerts.append(f"Portfolio history ends {last_ph:%Y-%m-%d}, but trades continue to {max(trades):%Y-%m-%d}. "
                      "Account-level charts (equity, returns, drawdown, benchmarks) only cover the history that "
                      "was fetched. Re-run <code>alpaca-analyzer fetch</code> with the latest version.")
    if not eq.attrs.get("funded", True):
        alerts.append("Every equity value in the portfolio history is $0 – the fetched window predates funding.")
    dropped = eq.attrs.get("dropped_flows") or []
    if dropped:
        tot = sum(v for _, v in dropped)
        alerts.append(f"{len(dropped)} deposits/withdrawals (net ${tot:,.2f}) fall after the end of the portfolio "
                      "history and could not be counted.")
    return alerts


def _symbol_scorecard(a: Analysis, marks: dict[str, float]) -> pd.DataFrame:
    t = a.trades
    if t.empty:
        return pd.DataFrame()
    rows = []
    income = a.cash_summary.get("income_by_symbol", {})
    op = a.open_positions
    for und, g in t.groupby("underlying"):
        rows.append({"underlying": und,
                     "stock_pnl": g.loc[g["asset"] == "stock", "pnl"].sum(),
                     "option_pnl": g.loc[g["asset"] == "option", "pnl"].sum(),
                     "trades": len(g), "wins": int((g["pnl"] > 0).sum())})
    df = pd.DataFrame(rows).set_index("underlying")
    if not op.empty:
        u = op.groupby("underlying")["unrealized_pl"].sum(min_count=1)
        df = df.reindex(df.index.union(u.index)).fillna({"stock_pnl": 0, "option_pnl": 0, "trades": 0, "wins": 0})
        df["unrealized"] = u.reindex(df.index).fillna(0.0)
    else:
        df["unrealized"] = 0.0
    df["dividends"] = pd.Series(income).reindex(df.index).fillna(0.0)
    df["total_pnl"] = df[["stock_pnl", "option_pnl", "unrealized", "dividends"]].sum(axis=1)

    # ---- buy & hold comparison: hold the underlying with the same peak capital, first entry -> now
    allf = a.ledger.closed_df()
    bh_rows = {}
    stock_entries = {}
    for e in a.ledger.events:
        if e.kind == "trade" and not e.is_option and e.qty > 0:
            stock_entries.setdefault(e.symbol, (e.ny_date, e.price))
    first_touch = {}
    for e in a.ledger.events:
        if e.kind == "trade":
            first_touch.setdefault(e.underlying, e.ny_date)
    peak_capital = _peak_capital(a.ledger)
    for und in df.index:
        d0 = first_touch.get(und)
        if d0 is None or (a.start and d0 < a.start):
            continue
        p0 = None
        closes = M.bar_closes(a.bundle.bars, und)
        if closes is not None and len(closes[closes.index <= d0]):
            p0 = float(closes[closes.index <= d0].iloc[-1])
        elif und in stock_entries and stock_entries[und][0] == d0:
            p0 = stock_entries[und][1]
        p1 = marks.get(und)
        cap = peak_capital.get(und, 0.0)
        if p0 and p1 and cap:
            bh_rows[und] = {"bh_start": d0, "bh_p0": p0, "bh_p1": p1, "peak_capital": cap,
                            "bh_return": p1 / p0 - 1, "bh_pnl": cap * (p1 / p0 - 1)}
    bh = pd.DataFrame.from_dict(bh_rows, orient="index")
    df = df.join(bh)
    if "bh_pnl" in df:
        df["edge_vs_bh"] = df["total_pnl"] - df["bh_pnl"]
    return df.sort_values("total_pnl")


def _peak_capital(ledger: Ledger) -> dict[str, float]:
    """Peak gross capital simultaneously committed per underlying (stock cost + option premium/risk)."""
    exposure: dict[str, float] = {}
    peak: dict[str, float] = {}
    lots_cost: dict[str, float] = {}
    pos: dict[str, list[list[float]]] = {}
    for e in ledger.events:
        if e.kind not in ("trade", "delivery", "expired", "assigned", "exercised"):
            continue
        book = pos.setdefault(e.symbol, [])
        q = e.qty
        while abs(q) > 1e-9 and book and (book[0][0] > 0) != (q > 0):
            take = min(abs(q), abs(book[0][0]))
            book[0][0] += take if book[0][0] < 0 else -take
            q += take if q < 0 else -take
            if abs(book[0][0]) < 1e-9:
                book.pop(0)
        if abs(q) > 1e-9:
            book.append([q, e.price])
        u = e.underlying
        tot = 0.0
        for sym, b in pos.items():
            if underlying_of(sym) == u:
                mult = 100.0 if parse_occ(sym) else 1.0
                tot += sum(abs(x[0]) * x[1] * mult for x in b)
        peak[u] = max(peak.get(u, 0.0), tot)
    return peak


def _reconcile(ledger: Ledger, bundle: Bundle) -> pd.DataFrame | None:
    """Ledger-derived open quantities vs. what Alpaca reports - flags gaps in the history."""
    if not bundle.positions:
        return None
    mine = {s: sum(l.qty for l in lots) for s, lots in ledger.open_lots.items()}
    theirs = {p["symbol"]: float(p["qty"]) for p in bundle.positions}
    rows = []
    for s in sorted(set(mine) | set(theirs)):
        a, b = mine.get(s, 0.0), theirs.get(s, 0.0)
        if abs(a - b) > 1e-6:
            rows.append({"symbol": s, "ledger_qty": a, "broker_qty": b, "diff": a - b})
    return pd.DataFrame(rows)
