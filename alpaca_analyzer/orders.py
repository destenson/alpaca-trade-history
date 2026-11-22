"""Open (unfilled) orders: what each one would do to the account if it filled.

Alpaca's ``/v2/orders?status=open&nested=true`` returns multi-leg option orders (``order_class``
``mleg``) with their legs nested, a signed net ``limit_price`` (positive = debit, negative = credit)
and a per-leg ``ratio_qty``. Single-leg orders carry their own symbol/side/qty. For every order this
module works out:

* option orders - payoff at expiration and today (Black-Scholes at the legs' snapshot IV) *at the
  limit price*, max profit/loss, breakevens, an IV-implied probability of profit, the current mid
  and natural price, and the same curves combined with what you already hold on that underlying;

Closing orders and stock orders are parsed (so they can be counted) but not projected.

Fees are not known until an order fills, so projections exclude them.
"""
from __future__ import annotations

import math
from dataclasses import dataclass, field
from datetime import date, datetime

import numpy as np

from .interpret import STOCK, Unit, _evaluate, name_group
from .ledger import NY, Ledger, classify, Combo
from .occ import parse_occ, underlying_of
from .optionsmath import bs_price, payoff_curves
from . import reprice as RP

DONE = {"filled", "canceled", "expired", "replaced", "rejected", "stopped", "suspended", "done_for_day",
        "pending_cancel"}


def _f(x) -> float | None:
    try:
        v = float(x)
    except (TypeError, ValueError):
        return None
    return v if math.isfinite(v) else None


@dataclass
class OrderLeg:
    symbol: str
    ratio: float              # signed per order unit: + buy, - sell (contracts or shares)
    intent: str               # open | close | ''
    bid: float | None = None
    ask: float | None = None
    iv: float | None = None
    greeks: dict = field(default_factory=dict)
    model: "RP.LegModel | None" = None

    @property
    def is_option(self) -> bool:
        return parse_occ(self.symbol) is not None

    @property
    def mid(self) -> float | None:
        if self.bid and self.ask:
            return (self.bid + self.ask) / 2
        return self.ask or self.bid or None


@dataclass
class PendingOrder:
    id: str
    submitted: datetime | None
    status: str
    order_type: str
    tif: str
    units: float              # remaining (unfilled) quantity of the order
    legs: list[OrderLeg]
    net: float | None         # signed price per unit: + debit, - credit (None: market order)
    net_is_estimate: bool = False
    stop: float | None = None
    attached: str = ""        # bracket/OTO children, described
    underlying: str = ""
    asset: str = "option"     # option | stock
    effect: str = "open"      # open | close | mixed
    strategy: str = ""
    # filled in by evaluate()
    spot: float | None = None
    mid_now: float | None = None
    natural: float | None = None
    max_gain: float = math.nan
    max_loss: float = math.nan
    breakevens: list[float] = field(default_factory=list)
    pop: float | None = None
    expiry: date | None = None
    curves: dict = field(default_factory=dict)
    est_open: float | None = None    # model value per unit at the valuation time and spot (+ debit)
    model: dict = field(default_factory=dict)   # inputs for in-page re-pricing

    @property
    def mult(self) -> float:
        return 100.0 if self.asset == "option" else 1.0

    @property
    def cash(self) -> float | None:
        """Cash the fill moves: negative = you pay."""
        return None if self.net is None else -self.net * self.units * self.mult

    @property
    def legs_label(self) -> str:
        parts = []
        for l in self.legs:
            q = l.ratio * self.units
            c = parse_occ(l.symbol)
            what = f"{c.expiry:%m/%d} {c.strike:g}{c.right}" if c else f"{l.symbol} sh"
            parts.append(f"{'+' if q > 0 else '−'}{abs(q):g} {what}")
        return ", ".join(parts)


# --------------------------------------------------------------------------- parsing
def _intent(pi: str | None) -> str:
    pi = pi or ""
    return "open" if pi.endswith("_to_open") else "close" if pi.endswith("_to_close") else ""


def parse_orders(raw: list[dict]) -> list[PendingOrder]:
    out: list[PendingOrder] = []
    for o in raw or []:
        if (o.get("status") or "") in DONE:
            continue
        qty = _f(o.get("qty")) or 0.0
        units = qty - (_f(o.get("filled_qty")) or 0.0)
        if units <= 0:
            continue
        typ = o.get("type") or o.get("order_type") or ""
        lim, stop = _f(o.get("limit_price")), _f(o.get("stop_price"))
        sub = o.get("submitted_at") or o.get("created_at")
        try:
            sub_t = datetime.fromisoformat(sub.replace("Z", "+00:00")[:26] + "+00:00") if sub else None
        except ValueError:
            sub_t = None
        common = dict(id=o.get("id", ""), submitted=sub_t, status=o.get("status", ""), order_type=typ,
                      tif=o.get("time_in_force", ""), units=units, stop=stop)
        if (o.get("order_class") or "") == "mleg":
            legs = [OrderLeg(l["symbol"], (_f(l.get("ratio_qty")) or 1.0) * (1 if l.get("side") == "buy" else -1),
                             _intent(l.get("position_intent")))
                    for l in o.get("legs") or [] if l.get("symbol")]
            if not legs:
                continue
            out.append(PendingOrder(legs=legs, net=lim, **common))
            continue
        sym = o.get("symbol")
        if not sym:
            continue
        sgn = 1 if o.get("side") == "buy" else -1
        price = lim if lim is not None else (stop if typ == "stop" else None)
        children = []
        for l in o.get("legs") or []:
            desc = l.get("type") or ""
            p = _f(l.get("limit_price")) or _f(l.get("stop_price"))
            if p is not None:
                children.append(f"{l.get('side', '')} {desc} ${p:,.2f}".strip())
        out.append(PendingOrder(legs=[OrderLeg(sym, float(sgn), _intent(o.get("position_intent")))],
                                net=None if price is None else sgn * price, attached="; ".join(children),
                                **common))
    for po in out:
        po.underlying = underlying_of(po.legs[0].symbol)
        po.asset = "option" if any(l.is_option for l in po.legs) else "stock"
    return out


# --------------------------------------------------------------------------- evaluation
def _pop(legs, spot, shares, share_cost, today, expiry, iv) -> float | None:
    """P(payoff at the first expiry > 0) for a lognormal underlying at implied vol ``iv`` (no drift)."""
    if not iv or not spot or expiry is None:
        return None
    T = max((expiry - today).days, 0) / 365.0 + 0.5 / 365.0
    s = iv * math.sqrt(T)
    xs = spot * np.exp(np.linspace(-6 * s, 6 * s, 2401))
    y = np.asarray(payoff_curves(legs, spot, today, shares, share_cost, xs=xs)["expiry"])
    z = (np.log(xs / spot) + 0.5 * s * s) / s
    cdf = 0.5 * (1 + np.vectorize(math.erf)(z / math.sqrt(2)))
    win = (y[:-1] + y[1:]) / 2 > 0
    return float(np.sum(np.diff(cdf)[win]))


def _units(po: PendingOrder) -> list[Unit]:
    """Order legs as interpret.Units with the net price spread over the legs (by mid where known)."""
    mult = po.mult
    mids = [l.mid for l in po.legs]
    us = []
    for l, m in zip(po.legs, mids):
        q = l.ratio * po.units
        us.append(Unit(STOCK if not l.is_option else l.symbol, q, q * mult * (m or 0.0),
                       q * mult * (m or 0.0), dict(l.greeks), l.iv))
    if po.net is not None:   # make the legs' costs add up to the order's limit price exactly
        gap = po.net * po.units * mult - sum(u.cost for u in us)
        for u in us:
            u.cost += gap / len(us)
    return us


def _position_units(open_positions, und: str, snapshots: dict, today: date) -> list[Unit]:
    if open_positions is None or open_positions.empty:
        return []
    us = []
    for _, r in open_positions.iterrows():
        sym = r["symbol"]
        if r["asset"] == "option" and underlying_of(sym) == und and parse_occ(sym).expiry >= today:
            snap = snapshots.get(sym, {})
            us.append(Unit(sym, float(r["qty"]), float(r["cost_basis"]),
                           float(r["market_value"]) if r["market_value"] == r["market_value"] else 0.0,
                           {}, snap.get("impliedVolatility")))
        elif r["asset"] == "stock" and sym == und:
            q = float(r["qty"])
            lot_q = math.copysign(100 * (abs(q) // 100), q)       # odd-lot shares aren't part of a strategy
            if lot_q:
                f = lot_q / q
                us.append(Unit(STOCK, lot_q, float(r["cost_basis"]) * f,
                               (float(r["market_value"]) if r["market_value"] == r["market_value"] else 0.0) * f))
    return us


def _grid(units: list[Unit], spot: float):
    strikes = [parse_occ(u.symbol).strike for u in units if u.symbol != STOCK]
    lo, hi = min(strikes + [spot]) * 0.85, max(strikes + [spot]) * 1.15
    return np.linspace(lo, hi, 241), np.linspace(0.01, max(strikes + [spot]) * 3, 1201)


def _name(units: list[Unit]) -> str:
    n = name_group(units)
    if n:
        return n
    legs = {}
    for u in units:
        if u.symbol != STOCK:
            legs[u.symbol] = legs.get(u.symbol, 0) + u.qty
    legs = {k: v for k, v in legs.items() if abs(v) > 1e-9}
    if not legs:
        return "Stock"
    return classify(Combo(0, "", datetime.now(NY), legs=legs, stock_held_at_open=0.0))


def _models_for(units: list[Unit], snapshots: dict, market, und: str, ivs: dict) -> dict:
    out = {}
    u_ = market.underlyings.get(und) if market else None
    for u in units:
        if u.symbol != STOCK and u.symbol not in out and u_ is not None:
            out[u.symbol] = RP.calibrate(u.symbol, snapshots.get(u.symbol) or {}, u_, market, ivs.get(und))
    return out


def curve_open(units: list[Unit], models: dict, xs, iv_shift: float = 0.0) -> list[float]:
    """P&L vs underlying price at the valuation time (next open), from calibrated leg models."""
    xs = np.asarray(xs, dtype=float)
    y = np.full(len(xs), -sum(u.cost for u in units))
    for u in units:
        if u.symbol == STOCK:
            y += u.qty * xs
        else:
            m = models[u.symbol]
            y += 100 * u.qty * bs_price(xs, m.strike, m.T, max(m.iv * (1 + iv_shift), 1e-3), m.right, r=RP.R)
    return y.round(2).tolist()


def model_payload(units: list[Unit], models: dict) -> dict:
    """Compact leg list for the page's JavaScript re-pricer: [strike, is_call, qty x100, T years, iv]."""
    legs, shares = [], 0.0
    for u in units:
        if u.symbol == STOCK:
            shares += u.qty
        else:
            m = models[u.symbol]
            legs.append([m.strike, 1 if m.right == "C" else 0, round(100 * u.qty, 6), round(m.T, 8), round(m.iv, 6)])
    return {"legs": legs, "shares": shares, "cost": round(sum(u.cost for u in units), 4)}


def evaluate(orders: list[PendingOrder], ledger: Ledger, open_positions, snapshots: dict, spot_of,
             today: date | None = None, market=None) -> list[PendingOrder]:
    """``market`` (reprice.Market): when given, spots are the latest prints and the "today" curve is
    the estimated value at the next open from calibrated leg IVs."""
    today = today or datetime.now(NY).date()
    und_ivs = RP.underlying_ivs(snapshots)
    for po in orders:
        for l in po.legs:
            snap = snapshots.get(l.symbol) or {}
            q = snap.get("latestQuote") or {}
            l.bid, l.ask = _f(q.get("bp")) or None, _f(q.get("ap")) or None
            if not (l.bid or l.ask):
                t = _f((snap.get("latestTrade") or {}).get("p"))
                l.bid = l.ask = t
            l.iv = _f(snap.get("impliedVolatility"))
            g = snap.get("greeks") or {}
            m = l.ratio * po.units * (100 if l.is_option else 1)
            l.greeks = ({k: (_f(g.get(k)) or 0.0) * m for k in ("delta", "theta", "vega")} if g else
                        ({"delta": m} if not l.is_option else {}))
        mu = market.underlyings.get(po.underlying) if market else None
        po.spot = (mu.spot if mu and mu.spot else None) or spot_of(po.underlying)
        mids = [l.mid for l in po.legs]
        if all(m is not None for m in mids):
            po.mid_now = sum(l.ratio * m for l, m in zip(po.legs, mids))
        if all((l.ask if l.ratio > 0 else l.bid) for l in po.legs):
            po.natural = sum(l.ratio * (l.ask if l.ratio > 0 else l.bid) for l in po.legs)
        if po.net is None and po.mid_now is not None:
            po.net, po.net_is_estimate = po.mid_now, True

        # open / close / mixed, from Alpaca's position intent or the ledger's current position
        effects = set()
        for l in po.legs:
            if l.intent:
                effects.add(l.intent)
                continue
            pos = sum(lot.qty for lot in ledger.open_lots.get(l.symbol, []))
            effects.add("close" if pos and (pos > 0) != (l.ratio > 0) else "open")
        po.effect = effects.pop() if len(effects) == 1 else "mixed"

        if po.asset == "stock" or po.effect == "close":
            continue

        units = _units(po)
        po.strategy = _name(units)
        exps = [parse_occ(l.symbol).expiry for l in po.legs if l.is_option]
        po.expiry = min(exps) if exps else None
        if not po.spot or po.net is None:
            continue
        pos = _position_units(open_positions, po.underlying, snapshots, today)
        allu = units + pos
        xs, risk_xs = _grid(allu, po.spot)
        g = _evaluate(units, po.strategy, po.spot, today, xs, risk_xs)
        po.max_gain, po.max_loss, po.breakevens = g.max_gain, g.max_loss, g.breakevens
        legs = [{"symbol": u.symbol, "qty": u.qty, "cost": u.cost, "iv": u.iv} for u in units if u.symbol != STOCK]
        ivs = [l.iv for l in po.legs if l.iv and l.is_option and parse_occ(l.symbol).expiry == po.expiry]
        if not ivs:   # legs without a model IV: borrow from other contracts on the underlying, same expiry first
            near = [(_f(v.get("impliedVolatility")), parse_occ(k)) for k, v in snapshots.items()
                    if parse_occ(k) and parse_occ(k).underlying == po.underlying and _f(v.get("impliedVolatility"))]
            ivs = [iv for iv, c in near if c.expiry == po.expiry] or [iv for iv, _ in near]
        po.pop = _pop(legs, po.spot, 0.0, 0.0, today, po.expiry, float(np.mean(ivs)) if ivs else None)
        po.curves = {"x": g.curve_x, "expiry": g.curve_expiry, "today": g.curve_today}
        if market is not None and po.underlying in market.underlyings:
            models = _models_for(allu, snapshots, market, po.underlying, und_ivs)
            for l in po.legs:
                l.model = models.get(l.symbol)
            po.curves["today"] = curve_open(units, models, g.curve_x)
            po.est_open = sum(l.ratio * l.model.value(po.spot) for l in po.legs if l.model)
            po.model = {"order": model_payload(units, models),
                        "pos": model_payload(pos, models) if pos else None}
        if pos:
            cur = _evaluate(pos, "Position", po.spot, today, xs, risk_xs)
            comb = _evaluate(allu, "Position + order", po.spot, today, xs, risk_xs)
            if po.model:
                comb.curve_today = curve_open(allu, models, g.curve_x)
            po.curves.update(position=cur.curve_expiry, combined=comb.curve_expiry,
                             combined_today=comb.curve_today,
                             pos_max_loss=cur.max_loss, pos_max_gain=cur.max_gain,
                             comb_max_loss=comb.max_loss, comb_max_gain=comb.max_gain,
                             comb_breakevens=comb.breakevens)
    return orders


def combine(orders: list[PendingOrder], open_positions, snapshots: dict, today: date | None = None,
            market=None) -> dict | None:
    """Every order on one underlying filling together (plus the current position)."""
    today = today or datetime.now(NY).date()
    os_ = [o for o in orders if o.asset == "option" and o.net is not None and o.spot]
    if len(os_) < 2:
        return None
    spot = os_[0].spot
    units = [u for o in os_ for u in _units(o)]
    pos = _position_units(open_positions, os_[0].underlying, snapshots, today)
    xs, risk_xs = _grid(units + pos, spot)
    g = _evaluate(units, "All orders", spot, today, xs, risk_xs)
    d = {"x": g.curve_x, "expiry": g.curve_expiry, "today": g.curve_today, "max_gain": g.max_gain,
         "max_loss": g.max_loss, "breakevens": g.breakevens, "net": sum(o.net * o.units * o.mult for o in os_),
         "spot": spot, "n": len(os_)}
    models = None
    if market is not None and os_[0].underlying in market.underlyings:
        models = _models_for(units + pos, snapshots, market, os_[0].underlying, RP.underlying_ivs(snapshots))
        d["today"] = curve_open(units, models, g.curve_x)
        d["est_open"] = sum(o.est_open * o.units * o.mult for o in os_ if o.est_open is not None)  # total $
        d["model"] = {"order": model_payload(units, models), "pos": model_payload(pos, models) if pos else None}
    if pos:
        cur = _evaluate(pos, "Position", spot, today, xs, risk_xs)
        comb = _evaluate(units + pos, "Position + orders", spot, today, xs, risk_xs)
        if models:
            comb.curve_today = curve_open(units + pos, models, g.curve_x)
        d.update(position=cur.curve_expiry, combined=comb.curve_expiry, combined_today=comb.curve_today,
                 comb_max_loss=comb.max_loss, comb_max_gain=comb.max_gain, comb_breakevens=comb.breakevens)
    return d
