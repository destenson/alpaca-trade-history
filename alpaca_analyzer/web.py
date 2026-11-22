"""Browser entry point (runs under Pyodide on the GitHub Pages site).

JavaScript fetches the account data from Alpaca, hands it over as one JSON document shaped like an
on-disk bundle, and gets back both the full and the shareable report HTML.
"""
from __future__ import annotations

import json

from . import analysis
from .bundle import Bundle
from .report import Report
from .share import make_shareable


def build_reports(bundle_json: str, options_json: str = "{}") -> str:
    d = json.loads(bundle_json)
    opts = json.loads(options_json or "{}")
    b = Bundle(activities=d.get("activities") or [], portfolio_history=d.get("portfolio_history"),
               positions=d.get("positions") or [], account=d.get("account"), bars=d.get("bars") or {},
               prices={k: float(v) for k, v in (d.get("prices") or {}).items()},
               option_snapshots=d.get("option_snapshots") or {}, orders=d.get("orders") or [],
               stock_snapshots=d.get("stock_snapshots") or {}, clock=d.get("clock"))
    if not b.activities:
        raise ValueError("This account has no activity yet - nothing to analyze.")
    a = analysis.run(b, benchmarks=opts.get("benchmarks") or ["SPY", "QQQ"],
                     combo_window_s=float(opts.get("combo_window", 120)), start=opts.get("start") or None)
    full = Report(a).to_html()
    share = Report(make_shareable(a, notional=float(opts.get("notional", 10_000)))).to_html()
    s = a.stats
    summary = None if s is None else {
        "equity": s.ending_equity, "pnl": s.pnl, "twr": s.total_return, "max_dd": s.max_dd,
        "start": str(s.start), "end": str(s.end)}
    return json.dumps({"full": full, "share": share, "summary": summary,
                       "warnings": a.warnings, "alerts": a.alerts}, default=str)
