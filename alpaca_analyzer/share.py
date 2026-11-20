"""Turn an :class:`Analysis` into a version that is safe to publish.

What is removed or transformed:

* **Account size**: every dollar figure is normalized. Account-level series become the growth of a
  ``notional`` account (deposits removed, time-weighted). Trade-level dollars are scaled to "per
  $notional of average equity", so relative sizes between trades survive but the real size doesn't.
  The scale factor itself is never rendered.
* **Deposits & withdrawals**: amounts and dates are gone (the notional account is funded once).
* **Open positions**: holdings, payoff diagrams, Greeks, unrealized P&L per name, and still-open
  option strategies are dropped.
* **Trade timing & size**: timestamps are rounded to the day, quantities are removed (also from leg
  labels such as ``+3 10/16 1C`` -> ``+ 10/16 1C``).
* **Free-text notes** that could carry quantities or prices (ledger warnings) are dropped.

Percent metrics (returns, drawdowns, Sharpe, win rates, return on risk, benchmark comparisons) are
unchanged.
"""
from __future__ import annotations

import copy
import re

import numpy as np
import pandas as pd

from . import metrics as M
from .analysis import Analysis

_LEG_QTY = re.compile(r"(?<![\w.])([+-]?)\d+(?:\.\d+)? (?=\d\d/\d\d )")


def strip_leg_qty(legs: str) -> str:
    """'+3 10/16 1C, -1 11/20 3C' -> '+ 10/16 1C, − 11/20 3C' (direction kept, size removed)."""
    return _LEG_QTY.sub(lambda m: ("−" if m.group(1) == "-" else "+") + " ", legs)


def _day(s: pd.Series) -> pd.Series:
    s = pd.to_datetime(s)
    return s.dt.normalize() if hasattr(s, "dt") else s


def make_shareable(a: Analysis, notional: float = 10_000.0) -> Analysis:
    s = copy.copy(a)
    s.share = True
    s.notional = notional

    # Scale for trade-level dollars: per `notional` of average equity over the window.
    if a.equity is not None and (a.equity["equity"] > 0).any():
        avg_cap = float(a.equity.loc[a.equity["equity"] > 0, "equity"].mean())
    elif not a.trades.empty:
        avg_cap = float(a.trades["cost"].sum()) or 1.0
    else:
        avg_cap = notional
    k = notional / avg_cap

    # ---- trades: scaled dollars, day-rounded times, no quantities
    t = a.trades.copy()
    if not t.empty:
        for c in ("pnl", "cost", "fees"):
            t[c] = t[c] * k
        t["open_time"], t["close_time"] = _day(t["open_time"]), _day(t["close_time"])
        t["hold_days"] = t["hold_days"].round(1)
        t["qty"] = np.nan
    s.trades = t

    # ---- option strategies: closed only, scaled, no sizes
    c = a.combos.copy() if a.combos is not None else pd.DataFrame()
    if not c.empty:
        c = c[~c["is_open"]].copy()
        for col in ("net_premium", "max_risk", "realized_pnl", "unrealized_pnl", "pnl"):
            c[col] = c[col] * k
        c["unrealized_pnl"] = 0.0
        c["legs"] = c["legs"].map(strip_leg_qty)
        c["open_time"], c["close_time"] = _day(c["open_time"]), _day(c["close_time"])
    s.combos = c

    # ---- per-symbol: realized only (unrealized would reveal current holdings)
    sy = a.symbols.copy() if a.symbols is not None else pd.DataFrame()
    if not sy.empty:
        for col in ("stock_pnl", "option_pnl", "dividends"):
            sy[col] = sy[col] * k
        sy["unrealized"] = 0.0
        sy["total_pnl"] = sy[["stock_pnl", "option_pnl", "dividends"]].sum(axis=1)
        sy = sy.drop(columns=[x for x in sy.columns if x.startswith("bh_") or x in ("peak_capital", "edge_vs_bh")])
        sy = sy[sy["trades"] > 0].sort_values("total_pnl")   # dividend-only names would reveal holdings
    s.symbols = sy

    # ---- open positions: gone. Keep only the aggregate unrealized for the attribution chart.
    op = a.open_positions
    unreal = float(op["unrealized_pl"].sum(min_count=1)) if op is not None and not op.empty else 0.0
    s.open_positions = pd.DataFrame(columns=op.columns if op is not None else [])

    cs = dict(a.cash_summary or {})
    s.cash_summary = {
        "fees": cs.get("fees", 0.0) * k, "fees_in_trades": cs.get("fees_in_trades", 0.0) * k,
        "income": cs.get("income", 0.0) * k, "interest": cs.get("interest", 0.0) * k,
        "flows": notional, "unrealized": (0.0 if np.isnan(unreal) else unreal) * k,
        "income_by_symbol": {kk: v * k for kk, v in (cs.get("income_by_symbol") or {}).items()},
    }

    # ---- account series: growth of `notional`, single funding, no deposit trail
    if a.equity is not None:
        e = pd.DataFrame(index=a.equity.index)
        e["equity"] = notional * a.equity["twr"]
        e["flow"] = 0.0
        e.iloc[0, e.columns.get_loc("flow")] = e["equity"].iloc[0]
        s.equity = M.add_returns(e)
        s.bench = {}
        for sym, bf in a.bench.items():
            nb = bf.copy()
            nb["value"] = notional * nb["twr"]
            nb["cum_pnl"] = nb["value"] - notional
            s.bench[sym] = nb
        pb = s.bench.get(a.primary_benchmark)
        s.stats = M.perf_stats(s.equity, pb)
        s.bench_stats = {}
        for sym, bf in s.bench.items():
            bdf = bf.rename(columns={"value": "equity"}).assign(flow=s.equity["flow"])
            bdf["pnl"] = bdf["equity"].diff().fillna(0)
            bdf["net_deposits"] = notional
            bdf["dd_dollar"] = bdf["cum_pnl"] - bdf["cum_pnl"].cummax()
            s.bench_stats[sym] = M.perf_stats(bdf, None)
        s.drawdowns = M.drawdown_periods(s.equity)
        s.rolling = M.rolling_stats(s.equity, pb)
        s.monthly = M.monthly_returns(s.equity)

    s.reconciliation = None if a.reconciliation is None else pd.DataFrame(
        {"matched": [a.reconciliation.empty]})
    s.warnings = []
    s.alerts = [re.sub(r" \(net \$[^)]*\)", "", x) for x in a.alerts]  # keep dates, drop amounts
    return s
