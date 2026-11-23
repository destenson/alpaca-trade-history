import math
from datetime import date

import pandas as pd
import pytest

from alpaca_analyzer.reinvest import reinvest_table


def combos(rows):
    return pd.DataFrame([{"underlying": u, "close_time": pd.Timestamp(t, tz="America/New_York"), "realized_pnl": p,
                          "is_open": False} for u, t, p in rows])


BARS = {"XYZ": [{"t": f"2026-01-{d:02d}T05:00:00Z", "c": c} for d, c in ((5, 40.0), (6, 50.0), (7, 25.0), (8, 30.0))]}


def test_wins_only_buys_whole_shares_and_carries_the_rest():
    c = combos([("XYZ", "2026-01-05 15:00", 90.0),     # 2 sh @ 40, 10 left
                ("XYZ", "2026-01-06 15:00", -60.0),    # ignored in the plan
                ("XYZ", "2026-01-07 15:00", 20.0)])    # 30 in the pot -> 1 sh @ 25, 5 left
    r = reinvest_table(c, BARS, lambda u: 30.0, {"XYZ": 1}).iloc[0]
    assert (r["wins"], r["losses"], r["profits"]) == (2, 1, 110.0)
    assert r["shares"] == 3 and r["cost"] == pytest.approx(105.0) and r["cash"] == pytest.approx(5.0)
    assert r["value"] == pytest.approx(90.0) and r["gain"] == pytest.approx(-15.0)
    assert r["short_by"] == 2                           # holds 1, plan says 3


def test_with_losses_variant_must_earn_losses_back():
    c = combos([("XYZ", "2026-01-05 15:00", 90.0), ("XYZ", "2026-01-06 15:00", -60.0),
                ("XYZ", "2026-01-07 15:00", 20.0)])
    r = reinvest_table(c, BARS, lambda u: 30.0, {}).iloc[0]
    # 90 -> 2 sh (10 left); -60 -> -50; +20 -> -30: nothing more bought
    assert r["net_shares"] == 2 and r["net_cash"] == pytest.approx(-30.0)


def test_closing_day_without_price_history_buys_nothing():
    c = combos([("ABC", "2026-01-05 15:00", 500.0)])
    r = reinvest_table(c, {}, lambda u: None, {}).iloc[0]
    assert r["shares"] == 0 and r["cash"] == 500.0 and r["unpriced"] == 1 and math.isnan(r["value"])
