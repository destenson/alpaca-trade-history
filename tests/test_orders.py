import math
from datetime import date

import pytest

from alpaca_analyzer import analysis
from alpaca_analyzer import orders as O
from alpaca_analyzer.bundle import Bundle, order_symbols
from alpaca_analyzer.report import Report

TODAY = date(2026, 11, 22)


def mleg(limit, legs, qty="1", status="accepted"):
    return {"id": "x", "order_class": "mleg", "type": "limit", "qty": qty, "filled_qty": "0",
            "limit_price": str(limit), "status": status, "time_in_force": "day",
            "submitted_at": "2026-11-22T00:07:42.812435195Z",
            "legs": [{"symbol": s, "side": side, "position_intent": f"{side}_to_{intent}", "ratio_qty": "1"}
                     for s, side, intent in legs]}


def snap(bp, ap, iv=0.5):
    return {"latestQuote": {"bp": bp, "ap": ap}, "impliedVolatility": iv}


def evaluate(raw, snaps, spot):
    b = Bundle(activities=[{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "1000"}])
    a = analysis.run(b)
    return O.evaluate(O.parse_orders(raw), a.ledger, a.open_positions, snaps, lambda u: spot, TODAY)


def test_debit_vertical_at_limit_price():
    (o,) = evaluate([mleg(2.5, [("AVGO261127P00317500", "buy", "open"), ("AVGO261127P00312500", "sell", "open")])],
                    {"AVGO261127P00317500": snap(16.3, 16.7), "AVGO261127P00312500": snap(13.3, 13.8)}, 304.0)
    assert o.strategy == "Bear put spread" and o.effect == "open"
    assert o.max_gain == pytest.approx(250) and o.max_loss == pytest.approx(-250)
    assert o.breakevens == [pytest.approx(315.0, abs=0.05)]
    assert o.mid_now == pytest.approx(16.5 - 13.55) and o.natural == pytest.approx(16.7 - 13.3)
    assert 0.5 < o.pop < 1          # spot already below the breakeven


def test_negative_limit_is_a_credit():
    (o,) = evaluate([mleg(-1.2, [("X261127P00100000", "sell", "open"), ("X261127P00095000", "buy", "open")])],
                    {}, 105.0)
    assert o.strategy == "Bull put spread"
    assert o.cash == pytest.approx(120)
    assert o.max_gain == pytest.approx(120) and o.max_loss == pytest.approx(-380)


def test_closing_and_done_orders_are_not_projected():
    raw = [mleg(-1.65, [("INTC261127C00051000", "buy", "close"), ("INTC261127C00049000", "sell", "close")]),
           mleg(1.0, [("X261127C00100000", "buy", "open"), ("X261127C00105000", "sell", "open")], status="filled"),
           {"id": "s", "order_class": "simple", "type": "limit", "symbol": "PACB", "side": "sell", "qty": "100",
            "filled_qty": "0", "limit_price": "16", "status": "new", "time_in_force": "gtc"}]
    os_ = evaluate(raw, {}, 50.0)
    assert len(os_) == 2                                   # the filled order is dropped
    assert os_[0].effect == "close" and math.isnan(os_[0].max_loss)
    assert os_[1].asset == "stock"


def test_single_leg_option_and_partial_fill():
    raw = [{"id": "a", "order_class": "simple", "type": "limit", "symbol": "AURA270416C00006000", "side": "buy",
            "qty": "3", "filled_qty": "1", "limit_price": "0.10", "status": "partially_filled", "time_in_force": "gtc"}]
    (o,) = evaluate(raw, {"AURA270416C00006000": snap(0.08, 4.8, 1.2)}, 6.8)
    assert o.units == 2 and o.strategy == "Long call"
    assert math.isinf(o.max_gain) and o.max_loss == pytest.approx(-20)


def test_order_symbols_and_report_tab():
    raw = [mleg(0.34, [("ZS261127C00220000", "buy", "open"), ("ZS261127C00225000", "sell", "open")]),
           mleg(1.5, [("ZS261127P00232500", "buy", "open"), ("ZS261127P00230000", "sell", "open")])]
    opts, unds = order_symbols(raw)
    assert unds == {"ZS"} and len(opts) == 4
    b = Bundle(activities=[{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "1000"}], orders=raw,
               prices={"ZS": 195.0})
    a = analysis.run(b)
    r = Report(a)
    out = r.to_html()
    assert 'id="orders"' in out and "ZS: all 2 orders together" in out
    from alpaca_analyzer.share import make_shareable
    assert 'id="orders"' not in Report(make_shareable(a)).to_html()
