"""Estimate option prices after the underlying has moved but the option quotes haven't.

Option quotes stop updating at the close (and over weekends), while the stock keeps trading in
extended hours and overnight. To estimate what a contract should be worth at the next open:

1. **Reference price** - the underlying price the option quotes reflect: the session close when the
   market is closed, the live price while it is open.
2. **Calibrate** - back out each leg's implied volatility from its own quoted mid at that reference
   price and the quote's timestamp (so the model reproduces the quote exactly). Unusable quotes
   (one-sided, very wide, or outside no-arbitrage bounds) fall back to Alpaca's IV, then to the
   average IV on the underlying.
3. **Re-price** - Black-Scholes at the new underlying price, with time measured to the valuation time
   (the next open when the market is closed, so weekend decay is included). Sticky strike: each
   strike keeps its own IV unless an IV change is applied.

European pricing with r = 4% and no dividends: fine for short-dated equity options, slightly
underprices deep-ITM American puts and calls ahead of dividends.
"""
from __future__ import annotations

import math
from dataclasses import dataclass
from datetime import date, datetime, time, timedelta, timezone

from .ledger import NY
from .occ import parse_occ
from .optionsmath import bs_price

R = 0.04
YEAR_S = 365.0 * 86400
STALE_MOVE = 0.02          # flag when the underlying moved this much since the option quotes
WIDE = 1.0                 # (ask - bid) / mid above this: the mid says little about value


def _f(x) -> float | None:
    try:
        v = float(x)
    except (TypeError, ValueError):
        return None
    return v if math.isfinite(v) and v > 0 else None


def _t(s) -> datetime | None:
    if not s:
        return None
    try:
        s = str(s).replace("Z", "+00:00")
        if "." in s:   # nanoseconds -> microseconds
            head, rest = s.split(".", 1)
            frac = "".join(ch for ch in rest if ch.isdigit())
            tz = rest[len(frac):]
            s = f"{head}.{frac[:6]}{tz}"
        d = datetime.fromisoformat(s)
        return d if d.tzinfo else d.replace(tzinfo=timezone.utc)
    except ValueError:
        return None


def expiry_time(d: date) -> datetime:
    return datetime.combine(d, time(16, 0), NY)


def years(t0: datetime, expiry: date) -> float:
    return max((expiry_time(expiry) - t0).total_seconds(), 60.0) / YEAR_S


def bs(S: float, K: float, T: float, iv: float, right: str) -> float:
    return float(bs_price(S, K, T, iv, right, r=R))


def implied_vol(price: float, S: float, K: float, T: float, right: str) -> float | None:
    disc = K * math.exp(-R * T)
    lo_b = max(S - disc, 0.0) if right == "C" else max(disc - S, 0.0)
    hi_b = S if right == "C" else disc
    if not (lo_b + 1e-4 < price < hi_b):
        return None
    lo, hi = 1e-3, 10.0
    for _ in range(100):
        m = (lo + hi) / 2
        if bs(S, K, T, m, right) > price:
            hi = m
        else:
            lo = m
    v = (lo + hi) / 2
    return v if 1e-3 < v < 9.9 else None


# --------------------------------------------------------------------------- market state
@dataclass
class Underlying:
    symbol: str
    close: float | None = None          # last regular-session close
    close_time: datetime | None = None
    last: float | None = None           # most recent trade on any feed
    last_time: datetime | None = None
    last_size: float | None = None
    last_feed: str = ""
    last_ext: bool = False              # extended-hours / overnight print
    ref: float | None = None            # price the option quotes reflect
    spot: float | None = None           # price used for estimates
    quote_time: datetime | None = None  # latest option quote on this underlying

    @property
    def move(self) -> float:
        return self.spot / self.ref - 1 if self.spot and self.ref else 0.0

    @property
    def stale(self) -> bool:
        return abs(self.move) >= STALE_MOVE


@dataclass
class Market:
    is_open: bool
    asof: datetime
    valuation: datetime                 # when estimates apply: now if open, else the next open
    underlyings: dict[str, Underlying]


def _next_open(now: datetime) -> datetime:
    d = now.astimezone(NY)
    cand = datetime.combine(d.date(), time(9, 30), NY)
    if d >= cand:
        cand += timedelta(days=1)
    while cand.weekday() >= 5:
        cand += timedelta(days=1)
    return cand


def market_state(clock: dict | None, stock_snapshots: dict | None, symbols, quote_times: dict,
                 fallback_prices: dict | None = None, now: datetime | None = None) -> Market:
    """``stock_snapshots``: {feed: {symbol: snapshot}}; the default feed is keyed "" or "default"."""
    clock = clock or {}
    now = _t(clock.get("timestamp")) or now or datetime.now(timezone.utc)
    if "is_open" in clock:
        is_open = bool(clock["is_open"])
    else:
        d = now.astimezone(NY)
        is_open = d.weekday() < 5 and time(9, 30) <= d.time() < time(16, 0)
    valuation = now if is_open else (_t(clock.get("next_open")) or _next_open(now))
    snaps = stock_snapshots or {}
    out: dict[str, Underlying] = {}
    for sym in sorted(set(symbols)):
        u = Underlying(sym, quote_time=quote_times.get(sym))
        base = (snaps.get("default") or snaps.get("") or {}).get(sym) or {}
        bar = base.get("dailyBar") or {}
        u.close = _f(bar.get("c"))
        if u.close and _t(bar.get("t")):
            u.close_time = datetime.combine(_t(bar["t"]).astimezone(NY).date(), time(16, 0), NY)
        for feed, book in snaps.items():
            tr = (book or {}).get(sym, {}).get("latestTrade") or {}
            t, p = _t(tr.get("t")), _f(tr.get("p"))
            if p and t and (u.last_time is None or t > u.last_time):
                conds = set(tr.get("c") or [])
                u.last, u.last_time, u.last_size = p, t, _f(tr.get("s"))
                u.last_feed = feed if feed not in ("", "default") else "primary"
                u.last_ext = (feed == "overnight" or bool({"T", "U"} & conds)
                              or (u.close_time is not None and t > u.close_time + timedelta(minutes=1)))
        fb = (fallback_prices or {}).get(sym)
        if is_open:
            u.ref = u.spot = u.last or u.close or fb
        else:
            u.ref = u.close or fb or u.last
            after = u.last if (u.last and u.close_time and u.last_time
                                and u.last_time > u.close_time + timedelta(minutes=1)) else None
            u.spot = after or u.ref
            if u.close_time is None and u.last:          # no session bar: just use the last print
                u.spot = u.last
        out[sym] = u
    return Market(is_open, now, valuation, out)


# --------------------------------------------------------------------------- legs
@dataclass
class LegModel:
    symbol: str
    strike: float
    right: str
    expiry: date
    iv: float
    iv_source: str        # "quote mid" | "Alpaca IV" | "underlying avg" | "default"
    T: float              # years from valuation time to expiry

    def value(self, S: float, iv_shift: float = 0.0) -> float:
        return bs(S, self.strike, self.T, max(self.iv * (1 + iv_shift), 1e-3), self.right)


def calibrate(symbol: str, snap: dict, und: Underlying, market: Market, fallback_iv: float | None) -> LegModel | None:
    c = parse_occ(symbol)
    if not c:
        return None
    q = snap.get("latestQuote") or {}
    bid, ask = _f(q.get("bp")), _f(q.get("ap"))
    qt = _t(q.get("t")) or market.asof
    iv, src = None, ""
    if bid and ask and und.ref:
        mid = (bid + ask) / 2
        if (ask - bid) / mid <= WIDE:
            iv = implied_vol(mid, und.ref, c.strike, years(min(qt, expiry_time(c.expiry)), c.expiry), c.right)
            src = "quote mid" if iv else ""
    if iv is None and _f(snap.get("impliedVolatility")):
        iv, src = float(snap["impliedVolatility"]), "Alpaca IV"
    if iv is None and fallback_iv:
        iv, src = fallback_iv, "underlying avg"
    if iv is None:
        iv, src = 0.5, "default"
    return LegModel(symbol, c.strike, c.right, c.expiry, iv, src, years(market.valuation, c.expiry))


def underlying_ivs(snapshots: dict) -> dict[str, float]:
    acc: dict[str, list[float]] = {}
    for sym, s in snapshots.items():
        c, iv = parse_occ(sym), _f((s or {}).get("impliedVolatility"))
        if c and iv:
            acc.setdefault(c.underlying, []).append(iv)
    return {k: sum(v) / len(v) for k, v in acc.items()}


def quote_times(snapshots: dict) -> dict[str, datetime]:
    out: dict[str, datetime] = {}
    for sym, s in snapshots.items():
        c, t = parse_occ(sym), _t(((s or {}).get("latestQuote") or {}).get("t"))
        if c and t and (c.underlying not in out or t > out[c.underlying]):
            out[c.underlying] = t
    return out
