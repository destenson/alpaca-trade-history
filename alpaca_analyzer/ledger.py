"""Turn raw Alpaca activities into a normalized event stream, then into lots, closed trades
and multi-leg option *strategies* via FIFO matching.

Conventions
-----------
* ``qty`` on events is the signed **position delta** (+ buys / - sells).
* Option expirations, assignments and exercises close the option at price 0. The stock leg
  of an assignment/exercise arrives separately (Alpaca ``OPTRD``) at the strike price, so the
  premium shows up on the option and the stock P&L is measured from the strike - the
  standard broker treatment.
* Cash amounts are signed from the account's point of view (+ = cash in).
"""
from __future__ import annotations

import math
from collections import defaultdict, deque
from dataclasses import dataclass, field
from datetime import date, datetime, time, timedelta
from zoneinfo import ZoneInfo

import pandas as pd

from .occ import parse_occ, underlying_of

NY = ZoneInfo("America/New_York")
UTC = ZoneInfo("UTC")
EPS = 1e-9

FLOW_TYPES = {"CSD", "CSW", "JNLC", "ACATC", "JNL"}
FEE_TYPES = {"FEE", "CFEE", "PTC", "DIVFEE"}
INCOME_TYPES = {"DIV", "DIVCGL", "DIVCGS", "DIVNRA", "DIVROC", "DIVTW", "DIVTXEX", "DIVFT", "CGD", "PTR"}
INTEREST_TYPES = {"INT", "INTNRA", "INTTW"}
OPTION_CLOSE = {"OPEXP": "expired", "OPASN": "assigned", "OPEXC": "exercised"}


def _f(x, default=0.0) -> float:
    try:
        return float(x)
    except (TypeError, ValueError):
        return default


def _parse_time(s: str) -> datetime:
    return pd.Timestamp(s).tz_convert(UTC).to_pydatetime() if pd.Timestamp(s).tzinfo else \
        pd.Timestamp(s).tz_localize(UTC).to_pydatetime()


def _close_of(d: date, hh: int = 16, mm: int = 0, ss: int = 0) -> datetime:
    return datetime.combine(d, time(hh, mm, ss), tzinfo=NY).astimezone(UTC)


# --------------------------------------------------------------------------- events
@dataclass
class Event:
    seq: int
    time: datetime          # UTC
    kind: str               # trade|expired|assigned|exercised|delivery|split|spin|flow|fee|income|interest|other
    symbol: str = ""
    qty: float = 0.0        # signed position delta
    price: float = 0.0
    cash: float = 0.0       # signed cash impact
    multiplier: float = 1.0
    order_id: str | None = None
    corp_id: str | None = None
    raw_type: str = ""
    sub_type: str = ""
    fee_date: date | None = None     # trade date a fee belongs to (fees post a day late)
    gross_price: float = math.nan    # broker fill price before fees (trades only)
    fee: float = 0.0                 # fees allocated to this fill (negative = cost)
    allocated: bool = False          # fee event already folded into trade costs

    @property
    def underlying(self) -> str:
        return underlying_of(self.symbol)

    @property
    def is_option(self) -> bool:
        return parse_occ(self.symbol) is not None

    @property
    def ny_date(self) -> date:
        return self.time.astimezone(NY).date()


def normalize(activities: list[dict]) -> list[Event]:
    """Convert raw activity dicts into time-ordered :class:`Event` objects."""
    evs: list[Event] = []
    seen_ids: set[str] = set()
    for i, a in enumerate(activities):
        aid = a.get("id")
        if aid:
            if aid in seen_ids:
                continue
            seen_ids.add(aid)
        t = (a.get("activity_type") or "").upper()
        sym = a.get("symbol") or ""
        if t == "FILL":
            side = (a.get("side") or "").lower()
            q = _f(a.get("qty"))
            q = q if side == "buy" else -q
            px = _f(a.get("price"))
            mult = 100.0 if parse_occ(sym) else 1.0
            evs.append(Event(i, _parse_time(a["transaction_time"]), "trade", sym, q, px, -q * px * mult, mult,
                             a.get("order_id"), raw_type=t, gross_price=px))
            continue

        d = date.fromisoformat((a.get("date") or a.get("created_at") or "")[:10])
        created = _parse_time(a["created_at"]) if a.get("created_at") else None
        net = _f(a.get("net_amount"))
        if t in OPTION_CLOSE:
            c = parse_occ(sym)
            when = min(d, c.expiry) if c else d
            # Exp/assign/exercise settle after the close: put them after the session's fills.
            ts = _close_of(when, 16, 0, 1)
            evs.append(Event(i, ts, OPTION_CLOSE[t], sym, _f(a.get("qty")), 0.0, net, 100.0, raw_type=t))
        elif t == "OPTRD":  # stock delivered by an assignment/exercise
            q = _f(a.get("qty"))
            px = _f(a.get("price")) or (abs(net / q) if q else 0.0)
            evs.append(Event(i, _close_of(d, 16, 0, 2), "delivery", sym, q, px, net or -q * px, 1.0, raw_type=t))
        elif t in ("SPLIT", "MA", "NC", "REORG"):
            evs.append(Event(i, created or _close_of(d, 9, 0), "split", sym, _f(a.get("qty")), _f(a.get("price")),
                             net, 1.0, corp_id=a.get("corporate_action_id"), raw_type=t,
                             sub_type=a.get("activity_sub_type") or ""))
        elif t == "SPIN":
            evs.append(Event(i, created or _close_of(d, 9, 0), "spin", sym, _f(a.get("qty")), _f(a.get("price")),
                             net, 1.0, corp_id=a.get("corporate_action_id"), raw_type=t))
        elif t in ("JNLS", "ACATS"):
            evs.append(Event(i, created or _close_of(d), "transfer_in", sym, _f(a.get("qty")), _f(a.get("price")),
                             0.0, 1.0, raw_type=t))
        elif t in FLOW_TYPES or t in ("CSD", "CSW"):
            evs.append(Event(i, _close_of(d, 9, 0), "flow", "", cash=net, raw_type=t))
        elif t in FEE_TYPES:
            evs.append(Event(i, _close_of(d, 17), "fee", sym, cash=net, order_id=a.get("order_id"), raw_type=t,
                             sub_type=(a.get("activity_sub_type") or "").upper(), fee_date=d))
        elif t in INCOME_TYPES:
            evs.append(Event(i, _close_of(d, 12), "income", sym, cash=net, raw_type=t))
        elif t in INTEREST_TYPES:
            evs.append(Event(i, _close_of(d, 17), "interest", sym, cash=net, raw_type=t))
        else:
            evs.append(Event(i, _close_of(d, 17), "other", sym, cash=net, raw_type=t))
    evs.sort(key=lambda e: (e.time, e.seq))
    return evs


# ------------------------------------------------------------------- fee allocation
def _fee_weights(sub_type: str, fills: list[Event]) -> list[float]:
    """How a fee that isn't tied to an order is spread over a day's fills."""
    notional = [abs(f.qty * f.gross_price * f.multiplier) for f in fills]
    contracts = [abs(f.qty) if f.is_option else 0.0 for f in fills]
    sells = [f.qty < 0 for f in fills]
    if sub_type in ("TAF", "REG"):                     # FINRA TAF / SEC fee: sells only
        w = [n if s_ else 0.0 for n, s_ in zip(notional, sells)]
    elif sub_type == "ORF":                            # options regulatory fee: per contract
        w = contracts
    elif sub_type == "OCC":                            # clearing fee: per contract
        w = contracts
    elif sub_type == "CAT":                            # consolidated audit trail: every execution
        w = [abs(f.qty) for f in fills]
    else:
        w = notional
    return w if sum(w) > 0 else [1.0] * len(fills)


def _apply_fee(fills: list[Event], weights: list[float], amount: float) -> None:
    tot = sum(weights)
    for f, w in zip(fills, weights):
        if w > 0:
            f.fee += amount * w / tot


def allocate_fees(events: list[Event], daily_fallback: dict[date, float] | None = None) -> None:
    """Fold fees into the fills they belong to, so lot prices are *effective* (cost / qty).

    * Fees carrying an ``order_id`` (e.g. OCC clearing) go to that order's fills, by quantity.
    * Daily fees (CAT/TAF/REG/ORF...) go to that trade date's fills, weighted by sub-type rules.
    * ``daily_fallback`` ({date: total}) is used only when no FEE activities exist at all
      (e.g. a CSV import); it is spread by contracts for options and per fill for stock.
    Fee events stay in the stream for reporting but are flagged ``allocated``.
    """
    trades = [e for e in events if e.kind == "trade"]
    by_order: dict[str, list[Event]] = defaultdict(list)
    by_date: dict[date, list[Event]] = defaultdict(list)
    for e in trades:
        if e.order_id:
            by_order[e.order_id].append(e)
        by_date[e.ny_date].append(e)

    fees = [e for e in events if e.kind == "fee"]
    for fe in fees:
        if fe.order_id and fe.order_id in by_order:
            fills = by_order[fe.order_id]
            _apply_fee(fills, [abs(f.qty) for f in fills], fe.cash)
            fe.allocated = True
            continue
        d = fe.fee_date or fe.ny_date
        fills = by_date.get(d) or by_date.get(_prev_trading_day_with_fills(by_date, d), [])
        if fe.symbol:
            fills = [f for f in fills if f.underlying == fe.symbol] or fills
        if fills:
            _apply_fee(fills, _fee_weights(fe.sub_type, fills), fe.cash)
            fe.allocated = True
    if not fees and daily_fallback:
        for d, amt in daily_fallback.items():
            fills = by_date.get(d) or by_date.get(_prev_trading_day_with_fills(by_date, d), [])
            if amt and fills:
                _apply_fee(fills, [abs(f.qty) if f.is_option else 1.0 for f in fills], amt)
    for e in trades:
        if e.fee:
            e.cash += e.fee
            e.price = -e.cash / (e.qty * e.multiplier)     # effective price incl. fees


def _prev_trading_day_with_fills(by_date: dict, d: date) -> date | None:
    prior = [x for x in by_date if x <= d and (d - x).days <= 4]
    return max(prior) if prior else None


# ---------------------------------------------------------------------------- lots
@dataclass
class Lot:
    symbol: str
    qty: float              # signed: + long, - short
    price: float
    time: datetime
    multiplier: float
    combo_id: int | None = None
    origin: str = "trade"
    fee_per_unit: float = 0.0


@dataclass
class ClosedTrade:
    symbol: str
    underlying: str
    asset: str              # stock | option
    direction: str          # long | short
    qty: float
    open_time: datetime
    close_time: datetime
    open_price: float
    close_price: float
    multiplier: float
    close_kind: str         # trade | expired | assigned | exercised | delivery
    combo_id: int | None
    origin: str = "trade"
    fees: float = 0.0       # open + close fees attributable to this lot (negative)

    @property
    def pnl(self) -> float:
        sgn = 1.0 if self.direction == "long" else -1.0
        return (self.close_price - self.open_price) * self.qty * self.multiplier * sgn

    @property
    def cost(self) -> float:
        return abs(self.open_price * self.qty * self.multiplier)


@dataclass
class Combo:
    """A group of option legs opened together (an option *strategy* position)."""
    id: int
    underlying: str
    open_time: datetime
    legs: dict[str, float] = field(default_factory=dict)          # symbol -> signed qty opened
    open_cash: float = 0.0                                          # + credit, - debit
    last_open_time: datetime | None = None
    rolled_from: int | None = None
    rolled_to: int | None = None
    stock_held_at_open: float = 0.0
    strategy: str = ""


@dataclass
class Ledger:
    events: list[Event]
    closed: list[ClosedTrade]
    open_lots: dict[str, list[Lot]]
    combos: dict[int, Combo]
    warnings: list[str]

    # ------------------------------------------------------------ dataframes
    def closed_df(self) -> pd.DataFrame:
        rows = [{
            "symbol": c.symbol, "underlying": c.underlying, "asset": c.asset, "direction": c.direction,
            "qty": c.qty, "open_time": c.open_time, "close_time": c.close_time, "open_price": c.open_price,
            "close_price": c.close_price, "multiplier": c.multiplier, "close_kind": c.close_kind,
            "combo_id": c.combo_id, "pnl": c.pnl, "cost": c.cost, "origin": c.origin, "fees": c.fees,
        } for c in self.closed]
        df = pd.DataFrame(rows)
        if df.empty:
            return df
        df["open_time"] = pd.to_datetime(df["open_time"], utc=True).dt.tz_convert(NY)
        df["close_time"] = pd.to_datetime(df["close_time"], utc=True).dt.tz_convert(NY)
        df["hold_days"] = (df["close_time"] - df["open_time"]).dt.total_seconds() / 86400
        df["ret"] = df["pnl"] / df["cost"].where(df["cost"] > 0)
        return df

    def open_df(self) -> pd.DataFrame:
        rows = []
        for sym, lots in self.open_lots.items():
            for l in lots:
                rows.append({"symbol": sym, "underlying": underlying_of(sym), "qty": l.qty, "price": l.price,
                             "time": l.time, "multiplier": l.multiplier, "combo_id": l.combo_id,
                             "asset": "option" if parse_occ(sym) else "stock"})
        return pd.DataFrame(rows)

    def cash_df(self) -> pd.DataFrame:
        rows = [{"time": e.time, "date": e.ny_date, "kind": e.kind, "symbol": e.symbol, "cash": e.cash,
                 "raw_type": e.raw_type, "allocated": e.allocated, "fee": e.fee}
                for e in self.events if e.cash or e.kind in ("flow", "fee", "income", "interest")]
        return pd.DataFrame(rows)


def build_ledger(events: list[Event], combo_window_s: float = 120.0) -> Ledger:
    lots: dict[str, deque[Lot]] = defaultdict(deque)
    closed: list[ClosedTrade] = []
    combos: dict[int, Combo] = {}
    warnings: list[str] = []
    forming: dict[str, Combo] = {}            # underlying -> combo still accepting legs
    next_combo = [1]
    pending_split: dict[str, list[Event]] = defaultdict(list)

    def position(sym: str) -> float:
        return sum(l.qty for l in lots[sym])

    def close_against(ev: Event, kind: str, qty_delta: float, price: float) -> float:
        """Close existing opposite lots FIFO; return the residual (opening) delta."""
        q = qty_delta
        dq = lots[ev.symbol]
        while abs(q) > EPS and dq and (dq[0].qty > 0) != (q > 0):
            lot = dq[0]
            take = min(abs(q), abs(lot.qty))
            close_fee = ev.fee * take / abs(ev.qty) if ev.qty else 0.0
            closed.append(ClosedTrade(
                ev.symbol, ev.underlying, "option" if ev.is_option else "stock",
                "long" if lot.qty > 0 else "short", take, lot.time, ev.time, lot.price, price,
                lot.multiplier, kind, lot.combo_id, lot.origin, lot.fee_per_unit * take + close_fee))
            lot.qty += take if lot.qty < 0 else -take
            q += take if q < 0 else -take
            if abs(lot.qty) < EPS:
                dq.popleft()
        return q

    def assign_combo(ev: Event, open_qty: float) -> int:
        und = ev.underlying
        c = forming.get(und)
        if c is None or (ev.time - (c.last_open_time or c.open_time)).total_seconds() > combo_window_s:
            c = Combo(next_combo[0], und, ev.time, stock_held_at_open=position(und))
            next_combo[0] += 1
            combos[c.id] = c
            forming[und] = c
        c.legs[ev.symbol] = c.legs.get(ev.symbol, 0.0) + open_qty
        c.open_cash += -open_qty * ev.price * ev.multiplier
        c.last_open_time = ev.time
        return c.id

    def flush_split(sym: str):
        evs = pending_split.pop(sym, [])
        if not evs:
            return
        removed = -sum(e.qty for e in evs if e.qty < 0)
        added = sum(e.qty for e in evs if e.qty > 0)
        held = position(sym)
        if removed > EPS and added > EPS:
            ratio = added / removed
        elif added > EPS and abs(held) > EPS:
            ratio = (held + added) / held
        elif removed > EPS and abs(held) > EPS and removed < held:
            ratio = (held - removed) / held
        else:
            warnings.append(f"{sym}: could not infer split ratio from {[(e.qty, e.raw_type) for e in evs]}")
            return
        for l in lots[sym]:
            l.qty *= ratio
            l.price /= ratio

    for ev in events:
        # Corporate-action rows for one action arrive as a pair; apply them together.
        for s in list(pending_split):
            if s != ev.symbol or ev.kind != "split":
                flush_split(s)
        if ev.kind == "split":
            pending_split[ev.symbol].append(ev)
            continue

        if ev.kind in ("trade", "delivery", "expired", "assigned", "exercised"):
            kind = "trade" if ev.kind == "trade" else ev.kind
            if ev.kind == "delivery":
                kind = "delivery"
            residual = close_against(ev, kind, ev.qty, ev.price)
            if abs(residual) > EPS:
                if ev.kind in ("expired", "assigned", "exercised"):
                    warnings.append(f"{ev.time:%Y-%m-%d} {ev.raw_type} {ev.symbol} qty {ev.qty:+g}: "
                                    f"no matching open position (history may start mid-position)")
                    continue
                combo_id = assign_combo(ev, residual) if ev.is_option else None
                lots[ev.symbol].append(Lot(ev.symbol, residual, ev.price, ev.time, ev.multiplier, combo_id,
                                           "assignment" if ev.kind == "delivery" else "trade",
                                           ev.fee / abs(ev.qty) if ev.qty else 0.0))
        elif ev.kind == "spin":
            lots[ev.symbol].append(Lot(ev.symbol, ev.qty, ev.price, ev.time, 1.0, None, "spinoff"))
            warnings.append(f"{ev.ny_date} spinoff {ev.symbol} x{ev.qty:g} booked at {ev.price:g} (basis not "
                            f"transferred from parent)")
        elif ev.kind == "transfer_in" and abs(ev.qty) > EPS:
            lots[ev.symbol].append(Lot(ev.symbol, ev.qty, ev.price, ev.time, 1.0, None, "transfer"))
    for s in list(pending_split):
        flush_split(s)

    # Roll detection (post-pass, so leg order within a multi-leg fill doesn't matter): a combo that
    # opens within the window of a *trade* closing another combo on the same underlying.
    for c in combos.values():
        best = None
        for ct in closed:
            if (ct.combo_id is None or ct.combo_id == c.id or ct.close_kind != "trade"
                    or ct.underlying != c.underlying or ct.combo_id > c.id):
                continue
            gap = abs((ct.close_time - c.open_time).total_seconds())
            if gap <= combo_window_s and (best is None or gap < best[0]):
                best = (gap, ct.combo_id)
        if best:
            c.rolled_from = best[1]
            combos[best[1]].rolled_to = combos[best[1]].rolled_to or c.id
    for c in combos.values():
        c.strategy = classify(c)
    return Ledger(events, closed, {k: list(v) for k, v in lots.items() if v}, combos, warnings)


# ------------------------------------------------------------------ classification
def classify(c: Combo) -> str:
    legs = [(parse_occ(s), q) for s, q in c.legs.items() if abs(q) > EPS]
    legs = [(k, q) for k, q in legs if k]
    if not legs:
        return "Unknown"
    n = len(legs)
    if n == 1:
        k, q = legs[0]
        if q > 0:
            return "Long call" if k.is_call else "Long put"
        if k.is_call:
            return "Covered call" if c.stock_held_at_open >= 100 * abs(q) - EPS else "Naked call"
        return "Short put"

    calls = [(k, q) for k, q in legs if k.is_call]
    puts = [(k, q) for k, q in legs if not k.is_call]
    same_exp = len({k.expiry for k, _ in legs}) == 1
    qs = {abs(q) for _, q in legs}

    if n == 2:
        (k1, q1), (k2, q2) = legs
        if k1.right == k2.right and (q1 > 0) != (q2 > 0):
            long_k, short_k = (k1, k2) if q1 > 0 else (k2, k1)
            if k1.expiry != k2.expiry:
                if long_k.strike == short_k.strike:
                    return "Calendar spread" if long_k.expiry > short_k.expiry else "Reverse calendar"
                return "Diagonal spread"
            if len(qs) > 1:
                return "Ratio spread"
            if long_k.is_call:
                return "Bull call spread" if long_k.strike < short_k.strike else "Bear call spread"
            return "Bear put spread" if long_k.strike > short_k.strike else "Bull put spread"
        if k1.right == k2.right:  # same side, same sign
            what = "2 strikes" if k1.strike != k2.strike else "2 expiries"
            return f"{'Long' if q1 > 0 else 'Short'} {'call' if k1.is_call else 'put'}s ({what})"
        # one call + one put
        cq = calls[0][1]
        pq = puts[0][1]
        if (cq > 0) == (pq > 0):
            side = "Long" if cq > 0 else "Short"
            return f"{side} straddle" if calls[0][0].strike == puts[0][0].strike and same_exp else f"{side} strangle"
        return "Risk reversal (synthetic long)" if cq > 0 else "Risk reversal (synthetic short)"

    if n == 3 and same_exp and (len(calls) == 3 or len(puts) == 3):
        srt = sorted(legs, key=lambda x: x[0].strike)
        if srt[0][1] > 0 and srt[2][1] > 0 and srt[1][1] < 0 and abs(srt[1][1]) == 2 * abs(srt[0][1]):
            return "Long butterfly"
        if srt[0][1] < 0 and srt[2][1] < 0 and srt[1][1] > 0:
            return "Short butterfly"

    if n == 4 and same_exp and len(calls) == 2 and len(puts) == 2 and len(qs) == 1:
        def vert(pair, is_call):
            (a, qa), (b, qb) = pair
            if (qa > 0) == (qb > 0):
                return None
            lng, sht = (a, b) if qa > 0 else (b, a)
            debit = lng.strike < sht.strike if is_call else lng.strike > sht.strike
            return "debit" if debit else "credit", sht.strike
        vc, vp = vert(calls, True), vert(puts, False)
        if vc and vp:
            if vc[0] == vp[0] == "credit":
                return "Iron butterfly" if vc[1] == vp[1] else "Iron condor"
            if vc[0] == vp[0] == "debit":
                return "Reverse iron condor"
            # one debit + one credit vertical: both point the same direction
            call_bias = "Bull" if vc[0] == "debit" else "Bear"
            return f"{call_bias}ish double vertical"

    side = "long" if all(q > 0 for _, q in legs) else "short" if all(q < 0 for _, q in legs) else "mixed"
    return f"Custom {n}-leg ({side})"


def max_risk(c: Combo) -> float:
    """Best-effort defined risk (in $) at entry; NaN when undefined (naked short legs)."""
    legs = [(parse_occ(s), q) for s, q in c.legs.items() if abs(q) > EPS]
    debit = -c.open_cash
    if all(q > 0 for _, q in legs):
        return debit
    s = c.strategy
    if s in ("Bull call spread", "Bear call spread", "Bear put spread", "Bull put spread"):
        width = abs(legs[0][0].strike - legs[1][0].strike) * abs(legs[0][1]) * 100
        return debit if debit > 0 else width - c.open_cash
    if s in ("Iron condor", "Iron butterfly", "Reverse iron condor") or s.endswith("double vertical"):
        calls = sorted([k.strike for k, _ in legs if k.is_call])
        puts = sorted([k.strike for k, _ in legs if not k.is_call])
        width = max(calls[1] - calls[0], puts[1] - puts[0]) * abs(legs[0][1]) * 100
        return debit if debit > 0 else width - c.open_cash
    if s in ("Calendar spread", "Diagonal spread", "Long butterfly") and debit > 0:
        return debit
    return math.nan
