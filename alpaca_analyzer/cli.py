"""Command line entry point.

    alpaca-analyzer fetch  --out data/live            # pull from the Alpaca API (read-only)
    alpaca-analyzer report --data data/live --out reports/report.html
    alpaca-analyzer run    --out reports/report.html  # fetch + report in one go
"""
from __future__ import annotations

import argparse
import sys
import urllib.request
from pathlib import Path

from . import analysis
from .bundle import Bundle, fetch_bundle
from .report import PLOTLY_CDN, render


def _report(bundle: Bundle, args) -> Path:
    a = analysis.run(bundle, benchmarks=args.benchmarks, combo_window_s=args.combo_window, start=args.start,
                     rf=args.rf)
    js = None
    if args.offline:
        with urllib.request.urlopen(PLOTLY_CDN, timeout=60) as r:  # embed plotly.js for offline viewing
            js = r.read().decode()
    out = render(a, args.out, inline_js=js)
    for w in a.warnings:
        print(f"note: {w}", file=sys.stderr)
    if a.stats:
        s = a.stats
        print(f"Equity {s.ending_equity:,.2f} | P&L {s.pnl:+,.2f} | TWR {s.total_return:+.2%} | "
              f"max DD {s.max_dd:.2%} | Sharpe {s.sharpe:.2f}")
    print(f"Report written to {out}")
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="alpaca-analyzer", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    def common_fetch(p):
        p.add_argument("--paper", action="store_true", help="use the paper-trading endpoint")
        p.add_argument("--since", help="only fetch activities after YYYY-MM-DD (default: everything)")
        p.add_argument("--feed", help="market data feed: sip (paid) or iex (free); default per your plan")

    def common_report(p):
        p.add_argument("--out", default="reports/report.html", help="HTML report path")
        p.add_argument("--benchmarks", nargs="+", default=["SPY", "QQQ"], help="benchmark tickers (first is primary)")
        p.add_argument("--start", help="analyze from YYYY-MM-DD (default: start of the funded equity history)")
        p.add_argument("--combo-window", type=float, default=120.0,
                       help="seconds within which option legs on one underlying form one strategy (default 120)")
        p.add_argument("--rf", type=float, default=0.0, help="annual risk-free rate for Sharpe/Sortino, e.g. 0.04")
        p.add_argument("--offline", action="store_true", help="embed plotly.js so the HTML works without internet")

    f = sub.add_parser("fetch", help="download account data into a bundle directory")
    f.add_argument("--out", default="data/live")
    f.add_argument("--benchmarks", nargs="+", default=["SPY", "QQQ"])
    common_fetch(f)

    r = sub.add_parser("report", help="build the HTML report from a bundle directory")
    r.add_argument("--data", default="data/live")
    common_report(r)

    run = sub.add_parser("run", help="fetch then report")
    run.add_argument("--data", default="data/live", help="where to cache the fetched bundle")
    common_fetch(run)
    common_report(run)

    args = ap.parse_args(argv)
    if args.cmd in ("fetch", "run"):
        from .client import AlpacaClient
        client = AlpacaClient(paper=args.paper)
        b = fetch_bundle(client, start=args.since, benchmarks=args.benchmarks, feed=args.feed)
        dest = args.out if args.cmd == "fetch" else args.data
        b.save(dest)
        print(f"Saved bundle to {dest}")
        if args.cmd == "fetch":
            return 0
        return 0 if _report(b, args) else 1
    return 0 if _report(Bundle.load(args.data), args) else 1


if __name__ == "__main__":
    sys.exit(main())
