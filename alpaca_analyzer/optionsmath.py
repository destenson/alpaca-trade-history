"""Black-Scholes pricing and payoff curves for open option positions."""
from __future__ import annotations

import math
from datetime import date

import numpy as np

from .occ import parse_occ


def _ncdf(x):
    return 0.5 * (1 + np.vectorize(math.erf)(x / math.sqrt(2)))


def bs_price(S, K, T, sigma, right, r=0.04):
    S = np.asarray(S, dtype=float)
    if T <= 0 or sigma <= 0:
        return np.maximum(S - K, 0) if right == "C" else np.maximum(K - S, 0)
    d1 = (np.log(S / K) + (r + 0.5 * sigma ** 2) * T) / (sigma * math.sqrt(T))
    d2 = d1 - sigma * math.sqrt(T)
    if right == "C":
        return S * _ncdf(d1) - K * math.exp(-r * T) * _ncdf(d2)
    return K * math.exp(-r * T) * _ncdf(-d2) - S * _ncdf(-d1)


def payoff_curves(legs: list[dict], spot: float, today: date, shares: float = 0.0,
                  share_cost: float = 0.0, n: int = 241, xs: np.ndarray | None = None) -> dict:
    """legs: [{symbol, qty, cost (total $ paid, signed), iv}] -> P&L curves vs underlying price.

    ``expiry`` curve: every leg at intrinsic value (the payoff once the nearest expiry has passed,
    approximating later-dated legs by intrinsic too). ``today`` curve: Black-Scholes with each
    leg's implied volatility.
    """
    strikes = [parse_occ(l["symbol"]).strike for l in legs]
    if xs is None:
        lo = min(strikes + [spot]) * 0.8
        hi = max(strikes + [spot]) * 1.2
        xs = np.linspace(lo, hi, n)
    xs = np.asarray(xs, dtype=float)
    n = len(xs)
    at_exp = np.full(n, -sum(l["cost"] for l in legs)) + shares * xs - share_cost
    now = at_exp.copy()
    for l in legs:
        c = parse_occ(l["symbol"])
        mult = 100 * l["qty"]
        at_exp += mult * (np.maximum(xs - c.strike, 0) if c.is_call else np.maximum(c.strike - xs, 0))
        T = max((c.expiry - today).days, 0) / 365.0 + 0.5 / 365.0
        iv = l.get("iv") or 0.5
        now += mult * bs_price(xs, c.strike, T, iv, c.right)
    sign = np.sign(at_exp)
    be = [float(xs[i] - at_exp[i] * (xs[i + 1] - xs[i]) / (at_exp[i + 1] - at_exp[i]))
          for i in range(n - 1) if sign[i] != 0 and sign[i] != sign[i + 1]]
    return {"x": xs.round(4).tolist(), "expiry": at_exp.round(2).tolist(), "today": now.round(2).tolist(),
            "breakevens": [round(b, 2) for b in be], "max_profit": float(at_exp.max()),
            "max_loss": float(at_exp.min())}
