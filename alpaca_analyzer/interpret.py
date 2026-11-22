"""Alternative readings of an open multi-leg position.

The same set of open legs can be grouped into strategies in more than one way - e.g. a 4-leg
"double vertical" is also a bear call spread + a bear put spread, or a synthetic short + a risk
reversal. Each reading has the same total P&L, but different per-group max loss, breakevens and
risk if only part of the position is closed. This module enumerates every partition of the legs
(and 100-share stock lots) into groups that are each a recognised strategy.
"""
from __future__ import annotations

import math
import re
from collections import Counter
from dataclasses import dataclass, field
from datetime import date, datetime, timezone

import numpy as np

from .ledger import Combo, classify
from .occ import parse_occ
from .optionsmath import payoff_curves

STOCK = "STOCK"
MAX_UNITS = 8          # Bell(8) = 4,140 partitions; beyond this whole legs are grouped instead
MAX_READINGS = 40
_TRIVIAL = re.compile(r"^(Long|Short) (calls|puts) \(")   # same-side legs lumped together add nothing


@dataclass
class Unit:
    symbol: str          # OCC symbol or STOCK
    qty: float           # signed: contracts (options) or shares (stock)
    cost: float          # $ paid (+) / received (-), fee-inclusive
    value: float         # current market value, signed
    greeks: dict = field(default_factory=dict)   # position greeks for this unit ($ / share-equivalents)
    iv: float | None = None


@dataclass
class Group:
    strategy: str
    units: list[Unit]
    defined_risk: bool
    max_gain: float      # inf = unlimited
    max_loss: float      # -inf = unlimited
    breakevens: list[float]
    cost: float
    value: float
    greeks: dict
    curve_x: list[float]
    curve_expiry: list[float]
    curve_today: list[float]

    @property
    def unrealized(self) -> float:
        return self.value - self.cost

    @property
    def legs_label(self) -> str:
        merged: dict[str, float] = {}
        for u in self.units:
            merged[u.symbol] = merged.get(u.symbol, 0) + u.qty
        parts = []
        for sym, q in sorted(merged.items(), key=lambda kv: _leg_sort(kv[0])):
            if sym == STOCK:
                parts.append(f"{'+' if q > 0 else '−'}{abs(q):g} sh")
            else:
                c = parse_occ(sym)
                parts.append(f"{'+' if q > 0 else '−'}{abs(q):g} {c.expiry:%m/%d} {c.strike:g}{c.right}")
        return ", ".join(parts)


@dataclass
class Reading:
    groups: list[Group]
    as_opened: bool = False

    @property
    def label(self) -> str:
        return " + ".join(g.strategy for g in self.groups)

    @property
    def undefined(self) -> int:
        return sum(not g.defined_risk for g in self.groups)

    @property
    def sum_max_loss(self) -> float:
        """Sum of each strategy group's own worst case."""
        return sum(g.max_loss for g in self.groups)


def _leg_sort(sym: str):
    if sym == STOCK:
        return (date.min, "", 0.0)
    c = parse_occ(sym)
    return (c.expiry, c.right, c.strike)


def _set_partitions(items: list):
    if not items:
        yield []
        return
    first, rest = items[0], items[1:]
    for part in _set_partitions(rest):
        for i in range(len(part)):
            yield part[:i] + [[first] + part[i]] + part[i + 1:]
        yield [[first]] + part


def name_group(units: list[Unit]) -> str:
    """Strategy name for a group of units, or '' if it isn't a recognised strategy."""
    legs: Counter = Counter()
    shares = 0.0
    for u in units:
        if u.symbol == STOCK:
            shares += u.qty
        else:
            legs[u.symbol] += u.qty
    legs = Counter({k: v for k, v in legs.items() if abs(v) > 1e-9})
    if shares:
        if shares < 0 or not legs:
            return "Stock" if not legs else ""
        calls = [(parse_occ(s), q) for s, q in legs.items() if parse_occ(s).is_call]
        puts = [(parse_occ(s), q) for s, q in legs.items() if not parse_occ(s).is_call]
        lots = shares / 100
        if len(calls) == 1 and not puts and calls[0][1] == -lots:
            return "Covered call"
        if len(puts) == 1 and not calls and puts[0][1] == lots:
            return "Protective put"
        if len(calls) == 1 and len(puts) == 1 and calls[0][1] == -lots and puts[0][1] == lots:
            return "Collar"
        return ""
    combo = Combo(0, "", datetime.now(timezone.utc), legs=dict(legs), stock_held_at_open=0.0)
    name = classify(combo)
    if name.startswith("Custom") or name == "Unknown" or _TRIVIAL.match(name):
        return ""
    return name


def _evaluate(units: list[Unit], strategy: str, spot: float, today: date, xs: np.ndarray,
              risk_xs: np.ndarray) -> Group:
    legs = [{"symbol": u.symbol, "qty": u.qty, "cost": u.cost, "iv": u.iv} for u in units if u.symbol != STOCK]
    shares = sum(u.qty for u in units if u.symbol == STOCK)
    share_cost = sum(u.cost for u in units if u.symbol == STOCK)
    shown = payoff_curves(legs, spot, today, shares, share_cost, xs=xs)
    risk = payoff_curves(legs, spot, today, shares, share_cost, xs=risk_xs)
    y = np.asarray(risk["expiry"])
    slope_hi = y[-1] - y[-2]
    max_gain = math.inf if slope_hi > 1e-6 else float(y.max())
    max_loss = -math.inf if slope_hi < -1e-6 else float(y.min())
    greeks: dict[str, float] = {}
    for u in units:
        for k, v in u.greeks.items():
            greeks[k] = greeks.get(k, 0.0) + v
    return Group(strategy, units, not math.isinf(max_loss), max_gain, max_loss, risk["breakevens"],
                 sum(u.cost for u in units), sum(u.value for u in units), greeks,
                 shown["x"], shown["expiry"], shown["today"])


def readings(units: list[Unit], spot: float, today: date, opened: list[list[str]] | None = None
             ) -> tuple[list[Reading], dict]:
    """All partitions of ``units`` into recognised strategies.

    ``units`` hold one contract (or 100 shares) each where possible so that a 2-lot leg can serve two
    spreads. ``opened`` is the as-opened grouping (lists of OCC symbols) used to flag that reading.
    Returns (readings, whole-position curves).
    """
    strikes = [parse_occ(u.symbol).strike for u in units if u.symbol != STOCK]
    lo, hi = min(strikes + [spot]) * 0.85, max(strikes + [spot]) * 1.15
    xs = np.linspace(lo, hi, 241)
    risk_xs = np.linspace(0.01, max(strikes + [spot]) * 3, 1201)
    whole = _evaluate(units, "Whole position", spot, today, xs, risk_xs)

    def key(group_units):  # identical contracts are interchangeable
        return tuple(sorted(Counter((u.symbol, 1 if u.qty > 0 else -1) for u in group_units).items()))

    seen, out = set(), []
    for part in _set_partitions(list(range(len(units)))):
        groups = [[units[i] for i in g] for g in part]
        canon = tuple(sorted(key(g) for g in groups))
        if canon in seen:
            continue
        seen.add(canon)
        # identical contracts split into separate piles aren't a different strategy
        pure = [g[0].symbol for g in groups if len({u.symbol for u in g}) == 1]
        if len(pure) != len(set(pure)):
            continue
        names = [name_group(g) for g in groups]
        if not all(names):
            continue
        out.append((canon, groups, names))
    opened_canon = None
    if opened:
        try:
            pool = list(units)
            ogroups = []
            for syms in opened:
                g = []
                for s in syms:
                    u = next(u for u in pool if u.symbol == s)
                    pool.remove(u)
                    g.append(u)
                ogroups.append(g)
            ogroups += [[u] for u in pool]   # anything not part of an opened combo (e.g. stock lots)
            opened_canon = tuple(sorted(key(g) for g in ogroups))
        except StopIteration:
            opened_canon = None

    rs = []
    for canon, groups, names in out:
        gs = [_evaluate(g, n, spot, today, xs, risk_xs) for g, n in zip(groups, names)]
        gs.sort(key=lambda g: min(_leg_sort(u.symbol) for u in g.units))
        rs.append(Reading(gs, as_opened=(canon == opened_canon)))
    rs.sort(key=lambda r: (not r.as_opened, r.undefined, len(r.groups), r.label))
    whole_d = {"x": whole.curve_x, "expiry": whole.curve_expiry, "today": whole.curve_today,
               "max_gain": whole.max_gain, "max_loss": whole.max_loss, "breakevens": whole.breakevens,
               "unrealized": whole.unrealized, "greeks": whole.greeks, "spot": spot}
    return rs[:MAX_READINGS], whole_d


def build_units(option_rows: list[dict], shares: float, share_cost: float, share_value: float,
                snapshots: dict) -> tuple[list[Unit], Unit | None, bool]:
    """Split positions into units of 1 contract / 100 shares when that keeps the count small."""
    total = sum(abs(r["qty"]) for r in option_rows) + int(abs(shares) // 100)
    split = total <= MAX_UNITS
    units: list[Unit] = []
    for r in option_rows:
        snap = snapshots.get(r["symbol"], {})
        g = snap.get("greeks") or {}
        n = int(abs(r["qty"])) if split else 1
        q = r["qty"] / n
        mult = 100 * q
        for _ in range(n):
            units.append(Unit(r["symbol"], q, r["cost_basis"] / n, (r["market_value"] or 0.0) / n,
                              {"delta": g.get("delta", 0) * mult, "theta": g.get("theta", 0) * mult,
                               "vega": g.get("vega", 0) * mult} if g else {},
                              snap.get("impliedVolatility")))
    # Only round lots (100 shares) take part in strategies; the odd remainder is returned separately so
    # callers can mention it, but it is never grouped or included in the position's payoff.
    sign = 1.0 if shares >= 0 else -1.0
    lots = int(abs(shares) // 100)
    per_share_cost = share_cost / shares if shares else 0.0
    per_share_val = share_value / shares if shares else 0.0
    if split:
        for _ in range(lots):
            q = 100 * sign
            units.append(Unit(STOCK, q, q * per_share_cost, q * per_share_val, {"delta": q}))
    elif lots:
        q = 100 * lots * sign
        units.append(Unit(STOCK, q, q * per_share_cost, q * per_share_val, {"delta": q}))
    rest = shares - 100 * lots * sign
    odd = Unit(STOCK, rest, rest * per_share_cost, rest * per_share_val, {"delta": rest}) if abs(rest) > 1e-9 else None
    return units, odd, split
