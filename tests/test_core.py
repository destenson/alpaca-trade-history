import math
from datetime import date

import pandas as pd
import pytest

from alpaca_analyzer import analysis
from alpaca_analyzer.bundle import Bundle
from alpaca_analyzer.ledger import allocate_fees, build_ledger, normalize
from alpaca_analyzer.metrics import add_returns
from alpaca_analyzer.occ import parse_occ


def fill(t, sym, side, qty, px, oid=None):
    return {"activity_type": "FILL", "transaction_time": t, "symbol": sym, "side": side,
            "qty": str(qty), "price": str(px), "order_id": oid}


def ledger_of(acts, **kw):
    evs = normalize(acts)
    allocate_fees(evs)
    return build_ledger(evs, **kw)


def test_parse_occ():
    c = parse_occ("NVDA261120P00152500")
    assert (c.underlying, c.expiry, c.right, c.strike) == ("NVDA", date(2026, 11, 20), "P", 152.5)
    assert parse_occ("AAPL") is None


def test_fifo_partial_and_flip():
    L = ledger_of([
        fill("2026-01-05T15:00:00Z", "XYZ", "buy", 10, 10),
        fill("2026-01-06T15:00:00Z", "XYZ", "buy", 10, 12),
        fill("2026-01-07T15:00:00Z", "XYZ", "sell", 15, 13),
    ])
    pnl = [round(c.pnl, 6) for c in L.closed]
    assert pnl == [30.0, 5.0]                      # 10 @10->13, 5 @12->13
    assert L.open_lots["XYZ"][0].qty == 5 and L.open_lots["XYZ"][0].price == 12


def test_fees_fold_into_effective_price():
    acts = [fill("2026-01-05T15:00:00Z", "ABC260116C00010000", "buy", 2, 1.00, "o1"),
            fill("2026-01-06T15:00:00Z", "ABC260116C00010000", "sell", 2, 1.50, "o2"),
            {"activity_type": "FEE", "activity_sub_type": "OCC", "order_id": "o1", "date": "2026-01-05",
             "net_amount": "-0.06"},
            {"activity_type": "FEE", "activity_sub_type": "TAF", "date": "2026-01-06", "net_amount": "-0.02"}]
    L = ledger_of(acts)
    (c,) = L.closed
    assert c.open_price == pytest.approx(1.0003)    # (200 + 0.06) / 200
    assert c.close_price == pytest.approx(1.4999)   # (300 - 0.02) / 200
    assert c.pnl == pytest.approx(100 - 0.08)
    assert all(e.allocated for e in L.events if e.kind == "fee")


def test_expiration_assignment_and_covered_call():
    acts = [fill("2026-08-03T14:00:00Z", "RXT", "buy", 100, 4.5),
            fill("2026-08-03T15:00:00Z", "RXT260807C00004000", "sell", 1, 0.75),
            {"activity_type": "OPASN", "date": "2026-08-07", "symbol": "RXT260807C00004000", "qty": "1",
             "net_amount": "0"},
            {"activity_type": "OPTRD", "date": "2026-08-07", "symbol": "RXT", "qty": "-100", "price": "4",
             "net_amount": "400"}]
    L = ledger_of(acts)
    combo = next(iter(L.combos.values()))
    assert combo.strategy == "Covered call"
    opt = [c for c in L.closed if c.asset == "option"][0]
    assert opt.close_kind == "assigned" and opt.pnl == pytest.approx(75)
    stk = [c for c in L.closed if c.asset == "stock"][0]
    assert stk.pnl == pytest.approx(-50)
    assert not L.open_lots


def test_reverse_split_preserves_basis():
    acts = [fill("2026-07-24T15:00:00Z", "GPUS", "buy", 1000, 0.124),
            {"activity_type": "SPLIT", "date": "2026-08-25", "created_at": "2026-08-25T05:59:00Z",
             "symbol": "GPUS", "qty": "-1000", "corporate_action_id": "x"},
            {"activity_type": "SPLIT", "date": "2026-08-25", "created_at": "2026-08-25T06:07:00Z",
             "symbol": "GPUS", "qty": "200", "corporate_action_id": "x"}]
    L = ledger_of(acts)
    (lot,) = L.open_lots["GPUS"]
    assert lot.qty == pytest.approx(200) and lot.price * lot.qty == pytest.approx(124)


@pytest.mark.parametrize("legs,expected", [
    ([("X260116C00100000", "buy"), ("X260116C00105000", "sell")], "Bull call spread"),
    ([("X260116P00100000", "buy"), ("X260116P00095000", "sell")], "Bear put spread"),
    ([("X260116P00100000", "sell"), ("X260116P00095000", "buy")], "Bull put spread"),
    ([("X260116C00100000", "buy"), ("X260116P00100000", "buy")], "Long straddle"),
    ([("X260116C00110000", "sell"), ("X260116C00115000", "buy"),
      ("X260116P00090000", "sell"), ("X260116P00085000", "buy")], "Iron condor"),
    ([("X260116C00100000", "sell"), ("X260220C00100000", "buy")], "Calendar spread"),
])
def test_strategy_classification(legs, expected):
    acts = [fill("2026-01-05T15:00:00Z", s, side, 1, 1.0) for s, side in legs]
    L = ledger_of(acts)
    assert [c.strategy for c in L.combos.values()] == [expected]


def test_roll_detected_regardless_of_leg_order():
    acts = [fill("2026-01-05T15:00:00Z", "X260116C00100000", "buy", 1, 2.0),
            # new leg listed before the closing leg at the same instant
            fill("2026-01-09T15:00:00Z", "X260123C00100000", "buy", 1, 2.5),
            fill("2026-01-09T15:00:00Z", "X260116C00100000", "sell", 1, 1.0)]
    L = ledger_of(acts)
    c2 = L.combos[2]
    assert c2.rolled_from == 1 and L.combos[1].rolled_to == 2


def test_time_weighted_return_ignores_deposits():
    df = pd.DataFrame({"equity": [1000, 1100, 2100, 2310], "flow": [1000, 0, 1000, 0]},
                      index=[date(2026, 1, d) for d in (5, 6, 7, 8)])
    r = add_returns(df)
    assert r["twr"].iloc[-1] == pytest.approx(1.1 * 1.0 * 1.1)
    assert r["cum_pnl"].iloc[-1] == pytest.approx(310)


def test_end_to_end_report(tmp_path):
    acts = [{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "10000"},
            fill("2026-01-06T15:00:00Z", "SPY", "buy", 10, 500),
            fill("2026-01-07T15:00:00Z", "SPY260116P00490000", "buy", 1, 3.0),
            fill("2026-01-08T15:00:00Z", "SPY", "sell", 10, 510),
            {"activity_type": "OPEXP", "date": "2026-01-16", "symbol": "SPY260116P00490000", "qty": "-1"}]
    days = pd.bdate_range("2026-01-05", "2026-01-20", tz="America/New_York")
    eq = [10000, 10000, 9990, 10097] + [10097] * (len(days) - 4)
    ts = [int((d + pd.Timedelta(hours=20)).timestamp()) for d in days]
    bars = {"SPY": [{"t": (d + pd.Timedelta(hours=4)).isoformat(), "c": 500 + i} for i, d in enumerate(days)]}
    b = Bundle(activities=acts, portfolio_history={"timestamp": ts, "equity": eq}, bars=bars)
    a = analysis.run(b, benchmarks=["SPY"])
    assert a.trades["pnl"].sum() == pytest.approx(100 - 300)
    assert a.combos.iloc[0]["outcome"] == "Expired"
    from alpaca_analyzer.report import render
    out = render(a, tmp_path / "r.html")
    html = out.read_text()
    assert "f_equity" in html and "f_strat" in html
