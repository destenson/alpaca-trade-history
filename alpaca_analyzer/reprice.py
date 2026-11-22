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
   (the next open when the market is closed). Sticky strike: each strike keeps its own IV unless an
   IV change is applied.

Time is counted in **trading days** (252 a year): only weekdays that aren't NYSE holidays accrue time,
24 hours each, so a weekend or holiday causes no decay. Friday close to Monday open is ~0.73 trading
days, not 2.73 calendar days.

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
TRADING_DAYS = 252.0
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


# --------------------------------------------------------------------------- trading calendar
def _easter(y: int) -> date:   # anonymous Gregorian algorithm
    a, b, c = y % 19, y // 100, y % 100
    d, e = b // 4, b % 4
    g = (8 * b + 13) // 25
    h = (19 * a + b - d - g + 15) % 30
    i, k = c // 4, c % 4
    l_ = (32 + 2 * e + 2 * i - h - k) % 7
    m = (a + 11 * h + 22 * l_) // 451
    month = (h + l_ - 7 * m + 114) // 31
    return date(y, month, (h + l_ - 7 * m + 114) % 31 + 1)


def _nth_weekday(y: int, month: int, weekday: int, n: int) -> date:
    d = date(y, month, 1)
    d += timedelta(days=(weekday - d.weekday()) % 7)
    return d + timedelta(weeks=n - 1)


def _last_weekday(y: int, month: int, weekday: int) -> date:
    d = date(y + (month == 12), month % 12 + 1, 1) - timedelta(days=1)
    return d - timedelta(days=(d.weekday() - weekday) % 7)


def _observed(d: date) -> date:
    return d - timedelta(days=1) if d.weekday() == 5 else d + timedelta(days=1) if d.weekday() == 6 else d


_HOLIDAYS: dict[int, set] = {}


def nyse_holidays(y: int) -> set:
    """Full-day NYSE closures by rule (early closes are treated as full days)."""
    if y not in _HOLIDAYS:
        hs = {_nth_weekday(y, 1, 0, 3), _nth_weekday(y, 2, 0, 3), _easter(y) - timedelta(days=2),
              _last_weekday(y, 5, 0), _observed(date(y, 7, 4)), _nth_weekday(y, 9, 0, 1),
              _nth_weekday(y, 11, 3, 4), _observed(date(y, 12, 25))}
        ny = date(y, 1, 1)
        if ny.weekday() != 5:          # NYSE doesn't move New Year's back into the prior year
            hs.add(_observed(ny))
        if y >= 2022:
            hs.add(_observed(date(y, 6, 19)))
        _HOLIDAYS[y] = hs
    return _HOLIDAYS[y]


def is_trading_day(d: date) -> bool:
    return d.weekday() < 5 and d not in nyse_holidays(d.year)


def trading_days(t0: datetime, t1: datetime) -> float:
    """Trading days between two instants: each weekday that isn't a holiday counts its 24 hours."""
    if t1 <= t0:
        return 0.0
    a, b = t0.astimezone(NY), t1.astimezone(NY)
    total, d = 0.0, a.date()
    while d <= b.date():
        if is_trading_day(d):
            start = max(a, datetime.combine(d, time(0), NY))
            end = min(b, datetime.combine(d + timedelta(days=1), time(0), NY))
            total += max((end - start).total_seconds(), 0.0) / 86400
        d += timedelta(days=1)
    return total


def years(t0: datetime, expiry: date) -> float:
    """Time to expiry (16:00 ET) in trading years; never below one minute of trading time."""
    return max(trading_days(t0, expiry_time(expiry)), 60 / 86400) / TRADING_DAYS


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
    while not is_trading_day(cand.date()):
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
