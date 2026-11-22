"""The on-disk *data bundle*: everything the analysis needs, fetched once and cached.

Layout of a bundle directory::

    activities.json         raw Alpaca account activities (FILL + non-trade), any order
    activities.csv          optional compact alternative/supplement (see README)
    portfolio_history.json  raw /v2/account/portfolio/history response (1D)
    positions.json          raw /v2/positions
    account.json            raw /v2/account
    bars.json               {symbol: [{"t": iso, "c": close}, ...]}  daily, adjusted
    prices.json             {symbol: last_price}  optional extra marks
    option_snapshots.json   {occ_symbol: snapshot}  optional (greeks / IV / quotes for open options and order legs)
    orders.json             raw /v2/orders?status=open&nested=true  optional (projections for unfilled orders)
    stock_snapshots.json    {feed: {symbol: snapshot}}  optional (after-hours / overnight prints for re-pricing)
    clock.json              raw /v2/clock  optional (market open? next open)
"""
from __future__ import annotations

import csv
import json
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

from .occ import parse_occ, underlying_of

NON_TRADE_FLOW = {"CSD", "CSW", "JNLC", "ACATC"}


@dataclass
class Bundle:
    activities: list[dict] = field(default_factory=list)
    portfolio_history: dict | None = None
    positions: list[dict] = field(default_factory=list)
    account: dict | None = None
    bars: dict[str, list[dict]] = field(default_factory=dict)
    prices: dict[str, float] = field(default_factory=dict)
    option_snapshots: dict[str, dict] = field(default_factory=dict)
    orders: list[dict] = field(default_factory=list)
    stock_snapshots: dict[str, dict] = field(default_factory=dict)
    clock: dict | None = None

    # ---------------------------------------------------------------- io
    @classmethod
    def load(cls, path: str | Path) -> "Bundle":
        p = Path(path)
        b = cls()

        def j(name, default):
            f = p / name
            return json.loads(f.read_text()) if f.exists() else default

        b.activities = list(j("activities.json", []))
        if (p / "activities.csv").exists():
            b.activities += read_activities_csv(p / "activities.csv")
        b.portfolio_history = j("portfolio_history.json", None)
        b.positions = j("positions.json", [])
        b.account = j("account.json", None)
        b.bars = j("bars.json", {})
        b.prices = {k: float(v) for k, v in j("prices.json", {}).items()}
        b.option_snapshots = j("option_snapshots.json", {})
        b.orders = j("orders.json", [])
        b.stock_snapshots = j("stock_snapshots.json", {})
        b.clock = j("clock.json", None)
        if not b.activities:
            raise FileNotFoundError(f"No activities found in {p} (expected activities.json or activities.csv)")
        return b

    def save(self, path: str | Path) -> None:
        p = Path(path)
        p.mkdir(parents=True, exist_ok=True)
        (p / "activities.json").write_text(json.dumps(self.activities, indent=1))
        if self.portfolio_history is not None:
            (p / "portfolio_history.json").write_text(json.dumps(self.portfolio_history))
        (p / "positions.json").write_text(json.dumps(self.positions, indent=1))
        if self.account is not None:
            (p / "account.json").write_text(json.dumps(self.account, indent=1))
        (p / "bars.json").write_text(json.dumps(self.bars))
        if self.prices:
            (p / "prices.json").write_text(json.dumps(self.prices, indent=1))
        if self.option_snapshots:
            (p / "option_snapshots.json").write_text(json.dumps(self.option_snapshots))
        if self.orders:
            (p / "orders.json").write_text(json.dumps(self.orders, indent=1))
        if self.stock_snapshots:
            (p / "stock_snapshots.json").write_text(json.dumps(self.stock_snapshots))
        if self.clock:
            (p / "clock.json").write_text(json.dumps(self.clock, indent=1))


def read_activities_csv(path: Path) -> list[dict]:
    """Compact CSV import. Columns (header required, extra columns ignored):

    ``activity_type,time,symbol,side,qty,price,net_amount,sub_type,order_id``

    * FILL rows need ``time`` (ISO-8601), ``side``, ``qty``, ``price``.
    * Non-trade rows need ``time`` (a date is fine) and the fields Alpaca would supply:
      ``net_amount`` for cash items, signed ``qty`` (+ ``price``) for OPEXP/OPASN/OPEXC/OPTRD/SPLIT/SPIN.
    """
    out = []
    with open(path, newline="") as fh:
        for row in csv.DictReader(r for r in fh if r.strip() and not r.lstrip().startswith("#")):
            row = {k.strip(): (v or "").strip() for k, v in row.items() if k}
            t = row["activity_type"].upper()
            rec: dict = {"activity_type": t}
            if t == "FILL":
                rec.update(transaction_time=row["time"], symbol=row["symbol"], side=row["side"],
                           qty=row["qty"], price=row["price"], order_id=row.get("order_id") or None,
                           type="fill")
            else:
                rec["date"] = row["time"][:10]
                if len(row["time"]) > 10:
                    rec["created_at"] = row["time"]
                for k in ("symbol", "qty", "price", "net_amount"):
                    if row.get(k):
                        rec[k] = row[k]
                if row.get("sub_type"):
                    rec["activity_sub_type"] = row["sub_type"]
                if row.get("order_id"):
                    rec["corporate_action_id"] = row["order_id"]
            out.append(rec)
    return out


def traded_symbols(activities: list[dict]) -> tuple[set[str], set[str]]:
    """(underlyings, option contracts) seen in fills/option events."""
    unds, opts = set(), set()
    for a in activities:
        sym = a.get("symbol")
        if not sym or a.get("activity_type") not in {"FILL", "OPTRD", "OPEXP", "OPASN", "OPEXC"}:
            continue
        if parse_occ(sym):
            opts.add(sym)
        unds.add(underlying_of(sym))
    return unds, opts


def fetch_bundle(client, start: str | None = None, benchmarks: list[str] | None = None,
                 feed: str | None = None, log=print) -> Bundle:
    """Pull everything from the Alpaca API into a :class:`Bundle`."""
    b = Bundle()
    log("Fetching account ...")
    b.account = client.account()
    log("Fetching activities (all pages) ...")
    b.activities = client.activities(after=start)
    log(f"  {len(b.activities)} activities")
    first = start or _first_activity_date(b.activities) or (date.today() - timedelta(days=365)).isoformat()
    log("Fetching portfolio history ...")
    b.portfolio_history = client.portfolio_history(start=f"{first}T00:00:00Z")
    ts = b.portfolio_history.get("timestamp") or []
    log(f"  {len(ts)} daily points")
    last_act = _last_activity_date(b.activities)
    if ts and last_act:
        last_ph = datetime.fromtimestamp(ts[-1], timezone.utc).date()
        if (date.fromisoformat(last_act) - last_ph).days > 7:
            log(f"  WARNING: portfolio history ends {last_ph} but activities run to {last_act}")
    log("Fetching positions ...")
    b.positions = client.positions()
    unds, _ = traded_symbols(b.activities)
    syms = sorted({s for s in unds if s.isalpha() or "." in s} | set(benchmarks or ["SPY", "QQQ"]))
    bar_start = (date.fromisoformat(first) - timedelta(days=7)).isoformat()
    log(f"Fetching daily bars for {len(syms)} symbols ...")
    b.bars = client.daily_bars(syms, start=f"{bar_start}T00:00:00Z", feed=feed)
    log("Fetching open orders ...")
    b.orders = client.open_orders()
    o_opts, o_unds = order_symbols(b.orders)
    open_opts = sorted({p["symbol"] for p in b.positions if p.get("asset_class") == "us_option"} | o_opts)
    if open_opts:
        log(f"Fetching snapshots for {len(open_opts)} option contracts (open positions + order legs) ...")
        b.option_snapshots = client.option_snapshots(open_opts)
    need = sorted((unds - set(b.bars)) | o_unds)
    if need:
        b.prices = client.latest_prices([s for s in need if s.isalpha() or "." in s])
    if o_unds:
        log("Fetching latest prints for order underlyings (primary + overnight feeds) ...")
        b.clock = client.clock()
        b.stock_snapshots = {"default": client.stock_snapshots(sorted(o_unds), feed=feed)}
        night = client.stock_snapshots(sorted(o_unds), feed="overnight")   # empty without a subscription
        if night:
            b.stock_snapshots["overnight"] = night
    return b


def order_symbols(orders: list[dict]) -> tuple[set[str], set[str]]:
    """(option contracts, underlyings) of open option orders - what their projections need priced."""
    opts: set[str] = set()
    for o in orders or []:
        for sym in [o.get("symbol")] + [l.get("symbol") for l in o.get("legs") or []]:
            if sym and parse_occ(sym):
                opts.add(sym)
    return opts, {underlying_of(s) for s in opts}


def _last_activity_date(acts: list[dict]) -> str | None:
    ds = [(a.get("transaction_time") or a.get("date") or "")[:10] for a in acts]
    ds = [d for d in ds if d]
    return max(ds) if ds else None


def _first_activity_date(acts: list[dict]) -> str | None:
    ds = [(a.get("transaction_time") or a.get("date") or "")[:10] for a in acts]
    ds = [d for d in ds if d]
    return min(ds) if ds else None
