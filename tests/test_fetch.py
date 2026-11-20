from datetime import datetime, timedelta, timezone

import pandas as pd

from alpaca_analyzer import analysis
from alpaca_analyzer.bundle import Bundle
from alpaca_analyzer.client import AlpacaClient


class FakeResp:
    def __init__(self, data):
        self.status_code, self._d, self.text = 200, data, ""

    def json(self):
        return self._d


def fake_history(params):
    """Mimic Alpaca: without `end`, only one month after `start` is returned."""
    start = datetime.fromisoformat(params["start"].replace("Z", "+00:00"))
    end = (datetime.fromisoformat(params["end"].replace("Z", "+00:00")) if "end" in params
           else start + timedelta(days=30))
    days = pd.bdate_range(start.date(), end.date())
    ts = [int(datetime(d.year, d.month, d.day, 20, tzinfo=timezone.utc).timestamp()) for d in days]
    return {"timestamp": ts, "equity": [1000.0] * len(ts), "cashflow": {"FEE": [-0.01] * len(ts)}}


def test_portfolio_history_covers_full_range(monkeypatch):
    c = AlpacaClient("k", "s")
    calls = []

    def get(url, params=None, timeout=None):
        calls.append(dict(params))
        return FakeResp(fake_history(params))

    monkeypatch.setattr(c.s, "get", get)
    h = c.portfolio_history("2024-01-01", end="2026-11-20T21:00:00Z")
    assert all("end" in p for p in calls) and len(calls) >= 3      # chunked, end always sent
    first = datetime.fromtimestamp(h["timestamp"][0], timezone.utc).date()
    last = datetime.fromtimestamp(h["timestamp"][-1], timezone.utc).date()
    assert str(first) == "2024-01-01" and str(last) == "2026-11-20"
    assert len(h["timestamp"]) == len(set(h["timestamp"])) == len(h["equity"]) == len(h["cashflow"]["FEE"])


def test_short_history_raises_alert_not_flat_chart():
    # history covers only an unfunded January; deposits & trades happen later
    days = pd.bdate_range("2026-01-02", "2026-01-30", tz="America/New_York")
    ph = {"timestamp": [int((d + pd.Timedelta(hours=20)).timestamp()) for d in days], "equity": [0.0] * len(days)}
    acts = [{"activity_type": "CSD", "date": "2026-06-22", "net_amount": "1000"},
            {"activity_type": "FILL", "transaction_time": "2026-07-01T15:00:00Z", "symbol": "XYZ", "side": "buy",
             "qty": "1", "price": "10"}]
    a = analysis.run(Bundle(activities=acts, portfolio_history=ph, account={"equity": "1000"}), benchmarks=["SPY"])
    text = " ".join(a.alerts)
    assert "Portfolio history ends 2026-01-30" in text
    assert "$0" in text and "could not be counted" in text
    assert a.equity.index[-1] == days[-1].date()        # no lone live point appended months later
