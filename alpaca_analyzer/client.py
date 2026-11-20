"""Minimal, read-only Alpaca REST client (Trading API v2 + Market Data API v2).

Only GET endpoints are used. Credentials come from the standard Alpaca env vars:
``APCA_API_KEY_ID`` / ``APCA_API_SECRET_KEY`` (``ALPACA_API_KEY`` / ``ALPACA_SECRET_KEY`` also accepted).
"""
from __future__ import annotations

import os
import time
from typing import Any, Iterable

import requests

LIVE_URL = "https://api.alpaca.markets"
PAPER_URL = "https://paper-api.alpaca.markets"
DATA_URL = "https://data.alpaca.markets"


class AlpacaError(RuntimeError):
    pass


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

    def portfolio_history(self, start: str, end: str | None = None, timeframe: str = "1D") -> dict:
        params = {"timeframe": timeframe, "start": start, "cashflow_types": "ALL",
                  "intraday_reporting": "market_hours", "pnl_reset": "no_reset"}
        if end:
            params["end"] = end
        return self._get(f"{self.base_url}/v2/account/portfolio/history", params)

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
