"""Chance that an unfilled option order's limit is reached, by horizon.

Monte Carlo on the underlying: geometric Brownian motion (no drift) at the legs' implied volatility,
starting from the re-pricing spot at the valuation time (now, or the next open). Time runs on the
trading-day clock used for re-pricing: each trading day is a 6.5-hour session followed by the overnight
gap, weekends and holidays add nothing. During sessions the order's combo is re-priced every half hour
with the calibrated leg models (Black-Scholes, time to expiry shrinking as the path advances); the
order counts as filled the first time that model value is at or through the limit:

    value (+ debit / − credit, per unit) <= limit (same sign convention)

That is the chance the *fair value* touches the limit, a proxy for a fill: real fills also depend on
the bid/ask spread and on market makers, so a limit at the mid fills less often than this suggests,
and one a little through the mid more often.
"""
from __future__ import annotations

import hashlib
import math
from datetime import date, datetime, time, timedelta

import numpy as np

from . import reprice as RP
from .ledger import NY

SESSION = 6.5 / 24            # one regular session in trading days
STEPS = 13                    # half-hour checks per session for the first sessions
STEPS_LATER = 3               # coarser checks once past the short horizons
PATHS = 2000                  # simulated paths (antithetic pairs)
HORIZONS = (1, 2, 3, 5, 8)    # trading sessions
GTC_DAYS = 90                 # Alpaca expires GTC orders after 90 days


def _ncdf(x: np.ndarray) -> np.ndarray:
    """Standard normal CDF (Abramowitz-Stegun 7.1.26 erf, |error| < 1.5e-7), vectorized."""
    z = np.abs(x) / math.sqrt(2)
    t = 1.0 / (1.0 + 0.3275911 * z)
    y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * np.exp(-z * z)
    return 0.5 * (1.0 + np.sign(x) * y)


def _bs(S: np.ndarray, K: float, T: float, iv: float, right: str) -> np.ndarray:
    if T <= 0:
        return np.maximum(S - K, 0.0) if right == "C" else np.maximum(K - S, 0.0)
    v = iv * math.sqrt(T)
    d1 = (np.log(S / K) + (RP.R + 0.5 * iv * iv) * T) / v
    d2 = d1 - v
    df = math.exp(-RP.R * T)
    if right == "C":
        return S * _ncdf(d1) - K * df * _ncdf(d2)
    return K * df * _ncdf(-d2) - S * _ncdf(-d1)


def sessions_until(start: datetime, end_day: date) -> int:
    """Trading sessions from ``start`` (a session open, or inside one) through ``end_day``'s session."""
    d, n = start.astimezone(NY).date(), 0
    while d <= end_day:
        if RP.is_trading_day(d):
            n += 1
        d += timedelta(days=1)
    return n


def fill_odds(po, market, iv_shift: float = 0.0) -> dict | None:
    """{'h1': p, 'h2': p, ... 'expiry': p, 'sessions': n} for an evaluated opening option order."""
    legs = [l for l in po.legs if l.is_option and l.model is not None]
    if po.net is None or not po.spot or not legs or len(legs) != len(po.legs):
        return None
    mu = market.underlyings.get(po.underlying)
    S0 = (mu.spot if mu and mu.spot else None) or po.spot
    t0 = market.valuation
    first = 1.0
    if market.is_open:       # only what's left of today's session counts as the first one
        close = datetime.combine(t0.astimezone(NY).date(), time(16, 0), NY)
        first = max((close - t0).total_seconds() / (6.5 * 3600), 0.02)
    gtc = (po.tif or "").lower() == "gtc"
    last_day = min(l.model.expiry for l in legs)
    if gtc and po.submitted:
        last_day = min(last_day, (po.submitted + timedelta(days=GTC_DAYS)).astimezone(NY).date())
    n_sess = max(sessions_until(t0, last_day), 1) if gtc else 1
    # vol driving the underlying: the legs' (calibrated) IVs, averaged
    sigma = max(float(np.mean([l.model.iv for l in legs])) * (1 + iv_shift), 0.05)

    seed = int(hashlib.sha1((po.id or po.legs_label).encode()).hexdigest()[:8], 16)
    rng = np.random.default_rng(seed)
    half = PATHS // 2
    logS = np.full(2 * half, math.log(S0))
    alive = np.ones(2 * half, dtype=bool)
    hit_at = np.full(2 * half, np.inf)            # session index (1-based) of the first touch

    def value(S, elapsed_td):
        v = np.zeros_like(S)
        for l in legs:
            T = max(l.model.T - elapsed_td / RP.TRADING_DAYS, 0.0)
            v += l.ratio * _bs(S, l.model.strike, T, max(l.model.iv * (1 + iv_shift), 1e-3), l.model.right)
        return v

    def step(dt_td):
        z = rng.standard_normal(half)
        z = np.concatenate([z, -z])                # antithetic pairs
        var = sigma * sigma * dt_td / RP.TRADING_DAYS
        return -0.5 * var + math.sqrt(var) * z

    elapsed = 0.0
    tol = 1e-9
    for k in range(1, n_sess + 1):
        sess = SESSION * (first if k == 1 else 1.0)
        m = STEPS if k <= max(HORIZONS) else STEPS_LATER
        checks = [0.0] + [sess / m] * m            # the open, then every half hour (or coarser)
        for dt in checks:
            if dt:
                logS += step(dt)
                elapsed += dt
            if not alive.any():
                break
            idx = np.where(alive)[0]
            v = value(np.exp(logS[idx]), elapsed)
            hit = v <= po.net + tol
            hit_at[idx[hit]] = k
            alive[idx[hit]] = False
        if k < n_sess:                              # overnight to the next open
            gap = 1.0 - SESSION                     # close -> next open (weekends/holidays add nothing)
            logS += step(gap)
            elapsed += gap
        if not alive.any():
            break
    out = {"sessions": n_sess, "gtc": gtc, "last_day": last_day}
    for h in HORIZONS:
        out[f"h{h}"] = float(np.mean(hit_at <= h)) if (gtc or h == 1) else None
    out["expiry"] = float(np.mean(np.isfinite(hit_at))) if gtc else None
    return out
