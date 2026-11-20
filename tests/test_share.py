import json
import re

import pandas as pd
import pytest

from alpaca_analyzer import analysis
from alpaca_analyzer.bundle import Bundle
from alpaca_analyzer.report import render
from alpaca_analyzer.share import make_shareable, strip_leg_qty


def fill(t, sym, side, qty, px):
    return {"activity_type": "FILL", "transaction_time": t, "symbol": sym, "side": side, "qty": str(qty),
            "price": str(px)}


def bundle():
    acts = [{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "73519"},
            {"activity_type": "CSD", "date": "2026-01-12", "net_amount": "4321"},
            fill("2026-01-06T15:07:00Z", "SPY", "buy", 37, 500),
            fill("2026-01-07T15:41:00Z", "SPY260116P00490000", "buy", 7, 3.0),
            fill("2026-01-08T16:22:00Z", "SPY", "sell", 37, 510),
            fill("2026-01-09T14:31:00Z", "SPY260116P00490000", "sell", 7, 4.0),
            fill("2026-01-13T14:31:00Z", "QQQ", "buy", 11, 400)]          # still open
    days = pd.bdate_range("2026-01-05", "2026-01-20", tz="America/New_York")
    eq = [73519.0, 73519, 73500, 73890, 74590, 74590, 78911] + [78911.0] * (len(days) - 7)
    ts = [int((d + pd.Timedelta(hours=20)).timestamp()) for d in days]
    bars = {s: [{"t": (d + pd.Timedelta(hours=4)).isoformat(), "c": p0 + i} for i, d in enumerate(days)]
            for s, p0 in (("SPY", 500), ("QQQ", 400))}
    pos = [{"symbol": "QQQ", "asset_class": "us_equity", "qty": "11", "avg_entry_price": "400",
            "cost_basis": "4400", "current_price": "405", "market_value": "4455", "unrealized_pl": "55"}]
    return Bundle(activities=acts, portfolio_history={"timestamp": ts, "equity": eq}, bars=bars, positions=pos,
                  account={"equity": "78966"})


def test_strip_leg_qty():
    assert strip_leg_qty("+3 10/16 1C, -1 11/20 3C") == "+ 10/16 1C, − 11/20 3C"
    assert strip_leg_qty("+1 11/23 130C") == "+ 11/23 130C"


def test_share_report_has_no_account_details(tmp_path):
    a = analysis.run(bundle(), benchmarks=["SPY"])
    s = make_shareable(a)
    html = render(s, tmp_path / "share.html").read_text()
    figs = re.search(r"const FIGS = (\{.*?\});\n", html, re.S).group(1)
    blob = re.sub(r"<script.*?</script>", "", html, flags=re.S) + figs
    for secret in ("73,519", "73519", "4,321", "4321", "78,911", "78911", "78,966", "78966", "×37", "×7", "4,400", "4400"):
        assert secret not in blob, secret
    assert not re.search(r"\d{4}-\d\d-\d\d \d\d:\d\d", figs)           # no intraday times
    assert ">Net premium<" not in html and ">Max risk<" not in html      # per-position $ would reveal scale
    assert 'data-tab="positions"' not in html and "Open option Greeks" not in html
    assert '"QQQ"' not in json.dumps([t for t in json.loads(figs).get("f_bysym", {}).get("data", [])])
    # percent metrics are unchanged by sharing
    assert s.stats.total_return == pytest.approx(a.stats.total_return)
    assert s.stats.max_dd == pytest.approx(a.stats.max_dd)
    assert s.stats.ending_equity == pytest.approx(10_000 * a.equity["twr"].iloc[-1])
