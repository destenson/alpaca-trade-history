from datetime import date, datetime, timezone

import pytest

from alpaca_analyzer import analysis
from alpaca_analyzer import orders as O
from alpaca_analyzer import reprice as RP
from alpaca_analyzer.bundle import Bundle
from alpaca_analyzer.report import Report

CLOCK_CLOSED = {"is_open": False, "next_open": "2026-11-23T09:30:00-05:00",
                "timestamp": "2026-11-22T12:31:09.208776118-05:00"}
FRI_QUOTE = "2026-11-20T20:59:59.5Z"


def stock(close, last, last_t, conds=("@", "T"), size=100):
    return {"dailyBar": {"c": close, "t": "2026-11-20T05:00:00Z"},
            "latestTrade": {"p": last, "t": last_t, "s": size, "c": list(conds)}}


def opt_snap(sym, S, iv, T_quote):
    """A quote whose mid is exactly Black-Scholes at (S, iv) - calibration should recover iv."""
    c = O.parse_occ(sym)
    mid = RP.bs(S, c.strike, RP.years(T_quote, c.expiry), iv, c.right)
    return {"latestQuote": {"bp": round(mid * 0.98, 4), "ap": round(mid * 1.02, 4), "t": FRI_QUOTE}}


def test_market_state_uses_after_hours_print_and_next_open():
    snaps = {"default": {"ZS": stock(194.83, 226.15, "2026-11-20T21:35:15.469443849Z"),
                         "ADI": stock(346.55, 346.55, "2026-11-20T20:59:58Z", ("@",))},
             "overnight": {"ZS": stock(199.97, 199.97, "2026-11-20T08:02:09Z", ("@",))}}   # older: ignored
    m = RP.market_state(CLOCK_CLOSED, snaps, ["ZS", "ADI"], {})
    zs, adi = m.underlyings["ZS"], m.underlyings["ADI"]
    assert (zs.ref, zs.spot, zs.last_ext, zs.stale) == (194.83, 226.15, True, True)
    assert zs.move == pytest.approx(226.15 / 194.83 - 1)
    assert (adi.spot, adi.stale) == (346.55, False)
    assert m.valuation.isoformat() == "2026-11-23T09:30:00-05:00"


def test_newer_overnight_print_wins():
    snaps = {"default": {"X": stock(100, 103, "2026-11-20T21:30:00Z")},
             "overnight": {"X": stock(101, 108, "2026-11-23T03:00:00Z", ("@",))}}
    u = RP.market_state(CLOCK_CLOSED, snaps, ["X"], {}).underlyings["X"]
    assert (u.spot, u.last_feed, u.last_ext) == (108, "overnight", True)


def test_market_open_means_no_repricing():
    snaps = {"default": {"X": stock(100, 103, "2026-11-23T15:00:00Z", ("@",))}}
    m = RP.market_state({"is_open": True, "timestamp": "2026-11-23T10:00:00-05:00"}, snaps, ["X"], {})
    assert m.underlyings["X"].ref == m.underlyings["X"].spot == 103 and not m.underlyings["X"].stale


def test_calibration_round_trips_the_quote_then_reprices():
    t_q = datetime(2026, 11, 20, 20, 59, 59, 500000, tzinfo=timezone.utc)
    sym = "ZS261127C00220000"
    snap = opt_snap(sym, 194.83, 0.7, t_q)
    m = RP.market_state(CLOCK_CLOSED, {"default": {"ZS": stock(194.83, 226.15, "2026-11-20T21:35:15Z")}}, ["ZS"],
                        {"ZS": t_q})
    lm = RP.calibrate(sym, snap, m.underlyings["ZS"], m, None)
    assert lm.iv_source == "quote mid" and lm.iv == pytest.approx(0.7, rel=1e-3)
    # at the reference price the model gives back the quoted mid (no time passing)
    mid = (snap["latestQuote"]["bp"] + snap["latestQuote"]["ap"]) / 2
    assert RP.bs(194.83, 220, RP.years(t_q, date(2026, 11, 27)), lm.iv, "C") == pytest.approx(mid, rel=1e-3)
    # at the new price it's worth far more; weekend decay alone lowers value at the old price
    assert lm.value(226.15) > 6 > mid
    assert lm.value(194.83) < mid
    assert lm.value(226.15, iv_shift=-0.4) < lm.value(226.15)


def test_unusable_quotes_fall_back():
    m = RP.market_state(CLOCK_CLOSED, {"default": {"A": stock(6.875, 6.875, "2026-11-20T20:59:00Z", ("@",))}}, ["A"], {})
    wide = {"latestQuote": {"bp": 0.08, "ap": 4.81, "t": FRI_QUOTE}, "impliedVolatility": 1.28}
    lm = RP.calibrate("A270416C00006000", wide, m.underlyings["A"], m, None)
    assert (lm.iv_source, lm.iv) == ("Alpaca IV", 1.28)
    lm = RP.calibrate("A270416C00006000", {}, m.underlyings["A"], m, 0.9)
    assert (lm.iv_source, lm.iv) == ("underlying avg", 0.9)


def _bundle():
    t_q = datetime(2026, 11, 20, 20, 59, 59, 500000, tzinfo=timezone.utc)
    legs = [("ZS261127C00220000", "buy"), ("ZS261127C00225000", "sell")]
    order = {"id": "z", "order_class": "mleg", "type": "limit", "qty": "1", "filled_qty": "0", "limit_price": "0.34",
             "status": "accepted", "time_in_force": "day", "submitted_at": "2026-11-22T00:07:42Z",
             "legs": [{"symbol": s, "side": side, "position_intent": f"{side}_to_open", "ratio_qty": "1"}
                      for s, side in legs]}
    snaps = {s: opt_snap(s, 194.83, 0.7, t_q) for s, _ in legs}
    return Bundle(activities=[{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "1000"}], orders=[order],
                  option_snapshots=snaps, clock=CLOCK_CLOSED,
                  stock_snapshots={"default": {"ZS": stock(194.83, 226.15, "2026-11-20T21:35:15Z")}})


def test_order_estimate_and_report_inputs():
    b = _bundle()
    a = analysis.run(b)
    os_ = O.evaluate(O.parse_orders(b.orders), a.ledger, a.open_positions, b.option_snapshots, lambda u: None,
                     date(2026, 11, 22), market=RP.market_state(b.clock, b.stock_snapshots, ["ZS"],
                                                                RP.quote_times(b.option_snapshots)))
    (o,) = os_
    assert o.spot == 226.15 and o.mid_now == pytest.approx(o.legs[0].mid - o.legs[1].mid)
    assert 2.0 < o.est_open < 5.0            # call spread now in the money, capped by the $5 width
    out = Report(a).to_html()
    assert 'id="rp-data"' in out and 'class="rp-spot"' in out and "Est. at open" in out
    assert "Moved since quotes" in out
