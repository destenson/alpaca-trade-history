"""'Reinvest option profits in the underlying': the whole shares option profits would have bought.

For each underlying, every closed option position adds its realized P&L to a pot on the day it closed.
Whenever the pot covers at least one share at that day's closing price, whole shares are bought and the
remainder stays in the pot. Two variants:

* **wins only** (the plan): only profitable positions feed the pot; losses are ignored.
* **with losses** (for comparison): losses come out of the same pot, so a loss has to be earned back
  before more shares are bought (the pot can go negative).
"""
from __future__ import annotations

import math
from dataclasses import dataclass, field
from datetime import date

import pandas as pd

from .ledger import NY
from .metrics import bar_closes


@dataclass
class Plan:
    shares: int = 0
    cost: float = 0.0
    cash: float = 0.0                      # left in the pot (negative: losses still to earn back)
    buys: list = field(default_factory=list)   # (date, shares, price)

    def add(self, d: date, amount: float, price: float | None):
        self.cash += amount
        if price and self.cash >= price:
            n = int(math.floor(self.cash / price + 1e-9))
            self.shares += n
            self.cost += n * price
            self.cash -= n * price
            self.buys.append((d, n, price))


def _day(ts) -> date:
    t = pd.Timestamp(ts)
    return (t.tz_convert(NY) if t.tzinfo else t).date()


def reinvest_table(combos: pd.DataFrame, bars: dict, price_now, held: dict) -> pd.DataFrame:
    """One row per underlying with closed option positions. ``price_now(und)`` gives today's price."""
    if combos is None or combos.empty:
        return pd.DataFrame()
    done = combos[~combos["is_open"]].copy()
    if done.empty:
        return pd.DataFrame()
    done["day"] = done["close_time"].map(_day)
    rows = []
    for und, g in done.sort_values("close_time").groupby("underlying", sort=False):
        closes = bar_closes(bars, und)
        wins, net = Plan(), Plan()
        unpriced = 0
        for d, pnl in zip(g["day"], g["realized_pnl"]):
            px = None
            if closes is not None:
                s = closes[closes.index <= d]
                px = float(s.iloc[-1]) if len(s) else None
            if px is None:
                unpriced += 1
            wins.add(d, max(float(pnl), 0.0), px)
            net.add(d, float(pnl), px)
        now = price_now(und)
        pnl = g["realized_pnl"].astype(float)
        rows.append({
            "underlying": und, "closed": len(g), "wins": int((pnl > 0).sum()), "losses": int((pnl < 0).sum()),
            "profits": float(pnl[pnl > 0].sum()), "loss_total": float(pnl[pnl < 0].sum()), "net": float(pnl.sum()),
            "shares": wins.shares, "cost": wins.cost, "avg_cost": wins.cost / wins.shares if wins.shares else math.nan,
            "cash": wins.cash, "price_now": now if now else math.nan,
            "value": wins.shares * now if now else math.nan,
            "gain": wins.shares * now - wins.cost if now and wins.shares else math.nan,
            "held": float(held.get(und, 0.0)),
            "net_shares": net.shares, "net_cash": net.cash,
            "net_value": net.shares * now if now else math.nan,
            "first_buy": wins.buys[0][0] if wins.buys else None, "buys": wins.buys, "unpriced": unpriced,
        })
    df = pd.DataFrame(rows)
    df["short_by"] = df["shares"] - df["held"]      # > 0: shares still to buy to match the plan
    return df.sort_values(["profits", "net"], ascending=False).reset_index(drop=True)
