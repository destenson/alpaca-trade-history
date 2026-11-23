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


def test_trading_day_clock_skips_weekends_and_holidays():
    from alpaca_analyzer.ledger import NY
    fri_close = datetime(2026, 11, 20, 16, 0, tzinfo=NY)
    mon_open = datetime(2026, 11, 23, 9, 30, tzinfo=NY)
    assert RP.trading_days(fri_close, mon_open) == pytest.approx(8 / 24 + 9.5 / 24)
    # Thanksgiving (Thu 11/26) accrues nothing: Mon open -> Fri 11/27 16:00 = 14.5h + Tue + Wed + 16h
    assert RP.trading_days(mon_open, RP.expiry_time(date(2026, 11, 27))) == pytest.approx(14.5 / 24 + 2 + 16 / 24)
    assert date(2026, 4, 3) in RP.nyse_holidays(2026)        # Good Friday
    assert date(2027, 6, 18) in RP.nyse_holidays(2027)       # Juneteenth on a Saturday -> Friday
    assert date(2027, 12, 31) not in RP.nyse_holidays(2027)  # New Year 2028 is a Saturday: no make-up day
    assert RP._next_open(datetime(2026, 11, 25, 17, 0, tzinfo=NY)).date() == date(2026, 11, 27)


def test_open_position_card_is_repriced():
    b = _bundle()
    b.orders = []
    sym_l, sym_s = "ZS261127C00220000", "ZS261127C00225000"
    b.activities += [
        {"activity_type": "FILL", "transaction_time": "2026-11-20T15:00:00Z", "symbol": sym_l, "side": "buy",
         "qty": "1", "price": "1.00", "order_id": "o1"},
        {"activity_type": "FILL", "transaction_time": "2026-11-20T15:00:00Z", "symbol": sym_s, "side": "sell",
         "qty": "1", "price": "0.60", "order_id": "o1"}]
    b.positions = [{"symbol": s, "asset_class": "us_option", "qty": q, "avg_entry_price": p, "current_price": m,
                    "market_value": str(float(q) * float(m) * 100), "cost_basis": str(float(q) * float(p) * 100),
                    "unrealized_pl": "0"} for s, q, p, m in ((sym_l, "1", "1.00", "0.95"), (sym_s, "-1", "0.60", "0.62"))]
    out = Report(analysis.run(b)).to_html()
    assert 'id="est_rd_0"' in out and "Whole position at next open (est.)" in out
    import json, re
    payload = json.loads(re.search(r'id="rp-data">(.*?)</script>', out).group(1).replace("<\\/", "</"))
    assert any(v["kind"] == "pos" and v["und"] == "ZS" for v in payload["views"])


def test_trades_tab_is_closed_positions_only():
    acts = [{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "10000"},
            {"activity_type": "FILL", "transaction_time": "2026-01-06T15:00:00Z", "symbol": "AAA", "side": "buy",
             "qty": "10", "price": "10"},
            {"activity_type": "FILL", "transaction_time": "2026-01-08T15:00:00Z", "symbol": "AAA", "side": "sell",
             "qty": "10", "price": "12"},
            {"activity_type": "FILL", "transaction_time": "2026-01-07T15:00:00Z", "symbol": "BBB", "side": "buy",
             "qty": "10", "price": "50"}]
    b = Bundle(activities=acts, positions=[{"symbol": "BBB", "asset_class": "us_equity", "qty": "10",
                                            "avg_entry_price": "50", "current_price": "80", "market_value": "800",
                                            "cost_basis": "500", "unrealized_pl": "300"}])
    a = analysis.run(b)
    assert a.symbols.loc["AAA", "realized"] == pytest.approx(20) and not a.symbols.loc["AAA", "is_open"]
    r = Report(a)
    r.to_html()
    fig = r.figs["f_bysym"]["data"][0]
    assert fig["y"] == ["AAA"] and fig["x"] == [20.0]          # BBB (open, unrealized +300) is not here
    assert a.symbols.loc["AAA", "bhc_end"] == date(2026, 1, 8)


def test_buy_and_hold_counts_partially_closed_names():
    acts = [{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "10000"},
            {"activity_type": "FILL", "transaction_time": "2026-01-06T15:00:00Z", "symbol": "CCC", "side": "buy",
             "qty": "10", "price": "10"},
            {"activity_type": "FILL", "transaction_time": "2026-01-08T15:00:00Z", "symbol": "CCC", "side": "sell",
             "qty": "5", "price": "12"},                                  # 5 closed, 5 still open
            {"activity_type": "FILL", "transaction_time": "2026-01-09T15:00:00Z", "symbol": "CCC", "side": "buy",
             "qty": "100", "price": "13"}]                                # after the last close: not in the window
    b = Bundle(activities=acts, positions=[{"symbol": "CCC", "asset_class": "us_equity", "qty": "105",
                                            "avg_entry_price": "12.9", "current_price": "13", "market_value": "1365",
                                            "cost_basis": "1350", "unrealized_pl": "15"}])
    s = analysis.run(b).symbols.loc["CCC"]
    assert s["is_open"] and s["realized"] == pytest.approx(10)
    assert s["bhc_end"] == date(2026, 1, 8) and s["bhc_capital"] == pytest.approx(100)
    assert s["bhc_pnl"] == pytest.approx(100 * (12 / 10 - 1))


def test_buy_and_hold_for_options_only_name_uses_bars():
    sym = "DDD260116C00050000"
    acts = [{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "10000"},
            {"activity_type": "FILL", "transaction_time": "2026-01-06T15:00:00Z", "symbol": sym, "side": "buy",
             "qty": "2", "price": "1.00"},
            {"activity_type": "FILL", "transaction_time": "2026-01-09T15:00:00Z", "symbol": sym, "side": "sell",
             "qty": "1", "price": "1.50"}]                                # half closed, half still open
    bars = {"DDD": [{"t": f"2026-01-{d:02d}T05:00:00Z", "c": c} for d, c in ((5, 48), (6, 50), (8, 52), (9, 55))]}
    b = Bundle(activities=acts, bars=bars,
               positions=[{"symbol": sym, "asset_class": "us_option", "qty": "1", "avg_entry_price": "1.0",
                           "current_price": "1.2", "market_value": "120", "cost_basis": "100", "unrealized_pl": "20"}])
    s = analysis.run(b).symbols.loc["DDD"]
    assert s["realized"] == pytest.approx(50) and s["bhc_end"] == date(2026, 1, 9)
    assert s["bhc_capital"] == pytest.approx(200) and s["bhc_pnl"] == pytest.approx(200 * (55 / 50 - 1))


def _evaluated(limit, tif="day"):
    b = _bundle()
    b.orders[0]["limit_price"] = str(limit)
    b.orders[0]["time_in_force"] = tif
    a = analysis.run(b)
    m = RP.market_state(b.clock, b.stock_snapshots, ["ZS"], RP.quote_times(b.option_snapshots))
    (o,) = O.evaluate(O.parse_orders(b.orders), a.ledger, a.open_positions, b.option_snapshots, lambda u: None,
                      date(2026, 11, 22), market=m)
    return o, m


def test_fill_odds_bounds_and_monotonic():
    from alpaca_analyzer.fillodds import fill_odds
    o, m = _evaluated(4.9)                      # far above the estimated value: fills at the open
    assert fill_odds(o, m)["h1"] == 1.0
    o, m = _evaluated(0.01)                     # far below: practically never within a day
    f = fill_odds(o, m)
    assert f["h1"] < 0.02 and f["h2"] is None and f["expiry"] is None
    o, m = _evaluated(round(o.est_open * 0.85, 2), "gtc")
    f = fill_odds(o, m)
    seq = [f["h1"], f["h2"], f["h3"], f["h5"], f["expiry"]]
    assert all(x <= y + 1e-12 for x, y in zip(seq, seq[1:])) and 0 < f["expiry"] < 1
