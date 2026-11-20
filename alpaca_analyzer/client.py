"""Minimal, read-only Alpaca REST client (Trading API v2 + Market Data API v2).

Only GET endpoints are used. Credentials come from the standard Alpaca env vars:
``APCA_API_KEY_ID`` / ``APCA_API_SECRET_KEY`` (``ALPACA_API_KEY`` / ``ALPACA_SECRET_KEY`` also accepted).
"""
from __future__ import annotations

import os
import time
from datetime import datetime, timedelta, timezone
from typing import Any, Iterable

import requests

LIVE_URL = "https://api.alpaca.markets"
PAPER_URL = "https://paper-api.alpaca.markets"
DATA_URL = "https://data.alpaca.markets"


class AlpacaError(RuntimeError):
    pass


def pd_ts(s: str) -> datetime:
    """Parse 'YYYY-MM-DD' or RFC 3339 into an aware UTC datetime."""
    s = s.strip().replace("Z", "+00:00")
    d = datetime.fromisoformat(s if "T" in s else s + "T00:00:00+00:00")
    return d if d.tzinfo else d.replace(tzinfo=timezone.utc)


def _rfc3339(d: datetime) -> str:
    return d.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class AlpacaClient:
    def __init__(self, key_id: str | None = None, secret: str | None = None, paper: bool = False,
                 base_url: str | None = None, data_url: str = DATA_URL, timeout: float = 30.0):
        key_id = key_id or os.getenv("APCA_API_KEY_ID") or os.getenv("ALPACA_API_KEY")
        secret = secret or os.getenv("APCA_API_SECRET_KEY") or os.getenv("ALPACA_SECRET_KEY")
        if not key_id or not secret:
            raise AlpacaError("Missing credentials: set APCA_API_KEY_ID and APCA_API_SECRET_KEY.")
        self.base_url = (base_url or os.getenv("APCA_API_BASE_URL") or (PAPER_URL if paper else LIVE_URL)).rstrip("/")
        if self.base_url.endswith("/v2"):
            self.base_url = self.base_url[:-3]
        self.data_url = data_url.rstrip("/")
        self.timeout = timeout
        self.s = requests.Session()
        self.s.headers.update({"APCA-API-KEY-ID": key_id, "APCA-API-SECRET-KEY": secret,
                               "Accept": "application/json", "User-Agent": "alpaca-analyzer/0.1"})

    # ------------------------------------------------------------------ http
    def _get(self, url: str, params: dict | None = None) -> Any:
        for attempt in range(5):
            r = self.s.get(url, params=params, timeout=self.timeout)
            if r.status_code == 429 or r.status_code >= 500:
                time.sleep(min(2 ** attempt, 20))
                continue
            if r.status_code >= 400:
                raise AlpacaError(f"GET {url} -> {r.status_code}: {r.text[:300]}")
            return r.json()
        raise AlpacaError(f"GET {url} failed after retries")

    # --------------------------------------------------------------- trading
    def account(self) -> dict:
        return self._get(f"{self.base_url}/v2/account")

    def positions(self) -> list[dict]:
        return self._get(f"{self.base_url}/v2/positions")

    def activities(self, after: str | None = None, until: str | None = None,
                   activity_types: Iterable[str] | None = None, page_size: int = 100) -> list[dict]:
        """All account activities (fills and non-trade), oldest first, following pagination."""
        out: list[dict] = []
        params: dict[str, Any] = {"direction": "asc", "page_size": page_size}
        if after:
            params["after"] = after
        if until:
            params["until"] = until
        if activity_types:
            params["activity_types"] = ",".join(activity_types)
        while True:
            page = self._get(f"{self.base_url}/v2/account/activities", params)
            if not page:
                break
            out.extend(page)
            if len(page) < page_size:
                break
            params["page_token"] = page[-1]["id"]
        return out

    def portfolio_history(self, start: str, end: str | None = None, timeframe: str = "1D",
                          chunk_days: int = 365) -> dict:
        """Daily equity history from ``start`` to ``end`` (default: now).

        Alpaca applies a default ``period`` of one month when only ``start`` is given, so ``end`` is
        always sent. The range is also requested in yearly windows and stitched together, which keeps
        each response small and independent of server-side defaults.
        """
        t0 = pd_ts(start)
        t1 = pd_ts(end) if end else datetime.now(timezone.utc)
        merged: dict = {"timestamp": [], "equity": [], "cashflow": {}, "timeframe": timeframe}
        seen: set[int] = set()
        cur = t0
        while cur < t1:
            nxt = min(cur + timedelta(days=chunk_days), t1)
            params = {"timeframe": timeframe, "start": _rfc3339(cur), "end": _rfc3339(nxt),
                      "cashflow_types": "ALL", "pnl_reset": "no_reset"}
            resp = self._get(f"{self.base_url}/v2/account/portfolio/history", params) or {}
            ts = resp.get("timestamp") or []
            eq = resp.get("equity") or []
            cf = resp.get("cashflow") or {}
            for i, t in enumerate(ts):
                if t in seen:
                    continue
                seen.add(t)
                merged["timestamp"].append(t)
                merged["equity"].append(eq[i] if i < len(eq) else None)
                n = len(merged["timestamp"])
                for k, arr in cf.items():
                    col = merged["cashflow"].setdefault(k, [])
                    col.extend([0.0] * (n - 1 - len(col)))
                    col.append(arr[i] if i < len(arr) else 0.0)
            cur = nxt
        n = len(merged["timestamp"])
        for col in merged["cashflow"].values():
            col.extend([0.0] * (n - len(col)))
        order = sorted(range(n), key=lambda i: merged["timestamp"][i])
        merged["timestamp"] = [merged["timestamp"][i] for i in order]
        merged["equity"] = [merged["equity"][i] for i in order]
        merged["cashflow"] = {k: [v[i] for i in order] for k, v in merged["cashflow"].items()}
        return merged

    # ------------------------------------------------------------------ data
    def daily_bars(self, symbols: list[str], start: str, end: str | None = None,
                   feed: str | None = None) -> dict[str, list[dict]]:
        """Split/dividend-adjusted daily bars keyed by symbol. Unknown symbols are skipped."""
        out: dict[str, list[dict]] = {}
        for i in range(0, len(symbols), 50):
            chunk = symbols[i:i + 50]
            params: dict[str, Any] = {"symbols": ",".join(chunk), "timeframe": "1Day", "start": start,
                                      "adjustment": "all", "limit": 10000}
            if end:
                params["end"] = end
            if feed:
                params["feed"] = feed
            while True:
                try:
                    resp = self._get(f"{self.data_url}/v2/stocks/bars", params)
                except AlpacaError:
                    if len(chunk) == 1:
                        break
                    # A bad symbol fails the whole request; retry one at a time.
                    for sym in chunk:
                        out.update(self.daily_bars([sym], start, end, feed))
                    break
                for sym, bars in (resp.get("bars") or {}).items():
                    out.setdefault(sym, []).extend({"t": b["t"], "c": b["c"]} for b in bars)
                tok = resp.get("next_page_token")
                if not tok:
                    break
                params["page_token"] = tok
        return out

    def option_snapshots(self, symbols: list[str]) -> dict[str, dict]:
        out: dict[str, dict] = {}
        for i in range(0, len(symbols), 100):
            params = {"symbols": ",".join(symbols[i:i + 100])}
            try:
                resp = self._get(f"{self.data_url}/v1beta1/options/snapshots", params)
            except AlpacaError:
                continue
            out.update(resp.get("snapshots") or {})
        return out

    def latest_prices(self, symbols: list[str]) -> dict[str, float]:
        out: dict[str, float] = {}
        for i in range(0, len(symbols), 100):
            try:
                resp = self._get(f"{self.data_url}/v2/stocks/snapshots", {"symbols": ",".join(symbols[i:i + 100])})
            except AlpacaError:
                continue
            for sym, snap in (resp or {}).items():
                if not snap:
                    continue
                px = (snap.get("latestTrade") or {}).get("p") or (snap.get("dailyBar") or {}).get("c")
                if px:
                    out[sym] = float(px)
        return out
