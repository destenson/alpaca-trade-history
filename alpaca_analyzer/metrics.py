"""Account-level performance: equity curve, cash-flow-adjusted returns, drawdowns,
risk ratios and benchmark comparison."""
from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np
import pandas as pd

from .ledger import NY, Ledger

TRADING_DAYS = 252


# ------------------------------------------------------------------ equity series
def equity_series(portfolio_history: dict | None, ledger: Ledger) -> pd.DataFrame:
    """Daily frame indexed by NY date with columns ``equity``, ``flow`` (external deposits/withdrawals).

    Flows come from the activity ledger (CSD/CSW/JNLC...) because they're dated exactly;
    Alpaca's own ``cashflow`` arrays are used as a fallback.
    """
    if not portfolio_history or not portfolio_history.get("timestamp"):
        raise ValueError("portfolio_history with daily timestamps is required for account-level metrics")
    ts = pd.to_datetime(portfolio_history["timestamp"], unit="s", utc=True).tz_convert(NY)
    # Alpaca stamps 1D points either at 00:00 ET or at the 20:00 ET session end of the trading
    # date they describe; in both cases the New York calendar date is the trading date.
    dates = ts.date
    eq = pd.Series(portfolio_history["equity"], index=pd.Index(dates, name="date"), dtype=float)
    eq = eq[~eq.index.duplicated(keep="last")]

    cash = ledger.cash_df()
    flows = pd.Series(0.0, index=eq.index)
    if not cash.empty and (cash["kind"] == "flow").any():
        f = cash[cash["kind"] == "flow"].groupby("date")["cash"].sum()
        for d, v in f.items():
            if d < eq.index[0]:
                continue  # before the equity history window
            # A flow dated on a non-trading day hits the next equity observation.
            idx = eq.index[eq.index >= d]
            if len(idx):
                flows[idx[0]] += v
    else:
        cf = portfolio_history.get("cashflow") or {}
        for k in ("CSD", "CSW", "JNLC", "ACATC"):
            if k in cf:
                flows += pd.Series(cf[k], index=eq.index).fillna(0).values
    df = pd.DataFrame({"equity": eq, "flow": flows})
    df["equity"] = df["equity"].ffill().fillna(0.0)

    # Start at the most recent funded stretch (skip leading zeros / long dormant gaps).
    unfunded = df.index[df["equity"] <= 1.0]
    if len(unfunded):
        last_unfunded = unfunded[-1]
        after = df.index[df.index > last_unfunded]
        if len(after):
            df = df.loc[after[0]:]
    # The opening balance counts as the first deposit.
    df = df.copy()
    first = df.index[0]
    df.loc[first, "flow"] = df.loc[first, "equity"] if df.loc[first, "flow"] == 0 else df.loc[first, "flow"]
    return df


def add_returns(df: pd.DataFrame) -> pd.DataFrame:
    """Modified-Dietz daily returns (flows assumed at start of day) -> time-weighted index."""
    out = df.copy()
    prev = out["equity"].shift(1).fillna(0.0)
    base = prev + out["flow"]
    pnl = out["equity"] - prev - out["flow"]
    out["pnl"] = pnl
    out["ret"] = np.where(base > 1e-6, pnl / base.where(base > 1e-6), 0.0)
    out.iloc[0, out.columns.get_loc("ret")] = 0.0 if abs(out["pnl"].iloc[0]) < 1e-9 else out["ret"].iloc[0]
    out["twr"] = (1 + out["ret"]).cumprod()
    out["net_deposits"] = out["flow"].cumsum()
    out["cum_pnl"] = out["equity"] - out["net_deposits"]
    out["dd"] = out["twr"] / out["twr"].cummax() - 1
    out["dd_dollar"] = out["cum_pnl"] - out["cum_pnl"].cummax()
    return out


# ------------------------------------------------------------------- benchmarks
def bar_closes(bars: dict, symbol: str) -> pd.Series | None:
    rows = bars.get(symbol)
    if not rows:
        return None
    s = pd.Series({pd.Timestamp(b["t"]).tz_convert(NY).date(): float(b["c"]) for b in rows})
    return s.sort_index()


def benchmark_frame(df: pd.DataFrame, closes: pd.Series) -> pd.DataFrame:
    """Simulate putting every deposit into the benchmark at that day's close (same cash flows)."""
    px = closes.reindex(closes.index.union(df.index)).sort_index().ffill().reindex(df.index)
    px = px.bfill()
    shares = (df["flow"] / px).cumsum()
    value = shares * px
    out = pd.DataFrame({"price": px, "value": value})
    out["cum_pnl"] = out["value"] - df["flow"].cumsum()
    out["ret"] = px.pct_change().fillna(0.0)
    out["twr"] = px / px.iloc[0]
    out["dd"] = out["twr"] / out["twr"].cummax() - 1
    return out


# ---------------------------------------------------------------------- stats
@dataclass
class PerfStats:
    start: object
    end: object
    days: int
    total_return: float
    cagr: float
    vol: float
    sharpe: float
    sortino: float
    max_dd: float
    max_dd_dollar: float
    calmar: float
    best_day: float
    worst_day: float
    pct_up_days: float
    net_deposits: float
    pnl: float
    ending_equity: float
    beta: float = math.nan
    alpha: float = math.nan
    correlation: float = math.nan
    up_capture: float = math.nan
    down_capture: float = math.nan
    tracking_error: float = math.nan
    info_ratio: float = math.nan


def perf_stats(df: pd.DataFrame, bench: pd.DataFrame | None = None, rf: float = 0.0) -> PerfStats:
    r = df["ret"].iloc[1:]
    n = len(r)
    years = max((pd.Timestamp(df.index[-1]) - pd.Timestamp(df.index[0])).days / 365.25, 1 / 365.25)
    total = df["twr"].iloc[-1] - 1
    cagr = (1 + total) ** (1 / years) - 1 if total > -1 else -1.0
    vol = r.std(ddof=1) * math.sqrt(TRADING_DAYS) if n > 1 else math.nan
    ex = r - rf / TRADING_DAYS
    sharpe = ex.mean() / r.std(ddof=1) * math.sqrt(TRADING_DAYS) if n > 1 and r.std() > 0 else math.nan
    downside = np.sqrt((np.minimum(ex, 0) ** 2).mean()) * math.sqrt(TRADING_DAYS) if n else math.nan
    sortino = ex.mean() * TRADING_DAYS / downside if downside and downside > 0 else math.nan
    mdd = df["dd"].min()
    st = PerfStats(
        df.index[0], df.index[-1], n, total, cagr, vol, sharpe, sortino, mdd, df["dd_dollar"].min(),
        cagr / abs(mdd) if mdd < 0 else math.nan, r.max() if n else math.nan, r.min() if n else math.nan,
        (r > 0).mean() if n else math.nan, df["flow"].sum(), df["cum_pnl"].iloc[-1], df["equity"].iloc[-1])
    if bench is not None and n > 5:
        b = bench["ret"].iloc[1:].reindex(r.index).fillna(0.0)
        if b.var() > 0:
            cov = np.cov(r, b)
            st.beta = cov[0, 1] / cov[1, 1]
            st.alpha = (r.mean() - st.beta * b.mean()) * TRADING_DAYS
            st.correlation = float(np.corrcoef(r, b)[0, 1])
            up, dn = b > 0, b < 0
            if up.any() and b[up].mean():
                st.up_capture = r[up].mean() / b[up].mean()
            if dn.any() and b[dn].mean():
                st.down_capture = r[dn].mean() / b[dn].mean()
            active = r - b
            st.tracking_error = active.std(ddof=1) * math.sqrt(TRADING_DAYS)
            st.info_ratio = active.mean() * TRADING_DAYS / st.tracking_error if st.tracking_error else math.nan
    return st


def drawdown_periods(df: pd.DataFrame, top: int = 5) -> pd.DataFrame:
    """Largest peak-to-trough episodes on the time-weighted index."""
    twr = df["twr"]
    peak = twr.cummax()
    under = twr < peak - 1e-12
    rows, start = [], None
    for d, u in under.items():
        if u and start is None:
            start = d
        if (not u or d == twr.index[-1]) and start is not None:
            seg = twr.loc[start:d]
            trough = seg.idxmin()
            pk_date = twr.loc[:start].idxmax()
            rows.append({"peak": pk_date, "trough": trough, "recovered": d if not u else None,
                         "depth": twr[trough] / peak[trough] - 1,
                         "length_days": (pd.Timestamp(d) - pd.Timestamp(pk_date)).days})
            start = None
    out = pd.DataFrame(rows)
    return out.sort_values("depth").head(top) if not out.empty else out


def rolling_stats(df: pd.DataFrame, bench: pd.DataFrame | None, window: int = 21) -> pd.DataFrame:
    r = df["ret"]
    out = pd.DataFrame(index=df.index)
    out["vol"] = r.rolling(window).std() * math.sqrt(TRADING_DAYS)
    out["sharpe"] = r.rolling(window).mean() / r.rolling(window).std() * math.sqrt(TRADING_DAYS)
    if bench is not None:
        b = bench["ret"].reindex(df.index).fillna(0)
        out["beta"] = r.rolling(window).cov(b) / b.rolling(window).var()
    return out


def monthly_returns(df: pd.DataFrame) -> pd.DataFrame:
    idx = pd.to_datetime(pd.Index(df.index))
    r = pd.Series(df["ret"].values, index=idx)
    pnl = pd.Series(df["pnl"].values, index=idx)
    m = pd.DataFrame({"ret": (1 + r).resample("ME").prod() - 1, "pnl": pnl.resample("ME").sum()})
    m["year"], m["month"] = m.index.year, m.index.month
    return m
