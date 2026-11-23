# alpaca-trade-history

Portfolio and trade analytics for an **Alpaca** brokerage account. It pulls your full activity history
(read-only), rebuilds every position with FIFO lot matching, groups option legs into strategies, and
writes one self-contained, interactive HTML report.

Try it now [in your browser](https://destenson.github.io/alpaca-trade-history/)

## What's in the report

| Tab | Contents |
|---|---|
| **Overview** | Equity vs. net deposits vs. SPY/QQQ bought with the *same deposits on the same days* · KPI tiles · P&L attribution waterfall (stocks, options, unrealized, dividends, interest) reconciled to the account's P&L |
| **Performance** | Time-weighted return vs. benchmarks · cumulative $ P&L vs. buy & hold · daily P&L · monthly returns · daily-return scatter with β/α, correlation, up/down capture · rolling 21-day volatility, Sharpe and beta · risk/return table (Sharpe, Sortino, Calmar, max DD) |
| **Drawdown** | Underwater curve vs. benchmarks · dollar drawdown · worst drawdown episodes |
| **Trades** | *Closed positions only.* Cumulative realized P&L (stocks/options) · P&L distribution · holding period vs. P&L · realized P&L by underlying · **your trading vs. just holding the underlying** (names fully exited, first trade → last exit) · P&L by entry weekday and hour · trade timeline · sortable table of every closed lot |
| **Options** | P&L and win rate by strategy · cumulative P&L by strategy · P&L by DTE at entry and by time in trade · calls vs. puts, debit vs. credit · how positions ended (closed, expired, assigned, exercised) · P&L by underlying · return-on-risk distribution · moneyness at entry · **roll chains** · **payoff diagrams for open positions** (at expiration + estimated value at the next open, re-priced like open orders with the same editable price/IV inputs; shares count in 100-share lots, odd lots are left out) · position Greeks · **option profits reinvested in the underlying** (whole shares each name's winning positions would have bought at the closing day's price, vs. what you hold; a with-losses variant for comparison) · strategy table |
| **Orders** | Unfilled option strategy orders projected **at your limit price**: risk/reward bars with IV-implied probability of profit for every order · table with mid, natural, edge vs mid, max profit/loss, reward/risk, breakevens · payoff explorer per order and for all orders on one underlying, overlaid on what you already hold there · **re-pricing when the stock moved after the close**: each leg's IV is calibrated to its own last quote, then re-priced (Black-Scholes) at the latest after-hours/overnight print and the time left at the next open (counted in trading days: weekends and NYSE holidays don't decay), with editable prices and IV changes per underlying that update estimates, table and charts live in the page. Closing and stock orders (take-profits) are counted, not projected. Never in the `--share` view. |
| **Positions** | Unrealized P&L by position · concentration · holdings table |
| *Every chart* | A **Log** button switches the value axis to a signed log scale: log for large values, −log(−x) for large negatives, linear within a small band around zero so it passes through 0; ticks and hover show real values; the choice is remembered per chart |
| **Method & data checks** | Ledger vs. broker position reconciliation, fee treatment, warnings |

Strategies detected: long/short calls and puts, covered calls (shares held at entry), cash-secured/short
puts, bull/bear call and put verticals, calendars, diagonals, straddles, strangles, risk reversals,
butterflies, iron condors/butterflies, reverse condors, double verticals, ratio spreads, and a
"custom N-leg" fallback.

## Quick start

```bash
pip install -e .
```

```bash
export APCA_API_KEY_ID=...        # a live-account key; only GET endpoints are called
export APCA_API_SECRET_KEY=...
```

```bash
alpaca-analyzer run --out reports/report.html
```

Open `reports/report.html` in a browser. `run` = `fetch` (download into `data/live/`) + `report`; you
can run them separately to re-render without hitting the API:

```bash
alpaca-analyzer fetch --out data/live
```

```bash
alpaca-analyzer report --data data/live --out reports/report.html --benchmarks SPY QQQ IWM
```

Useful options:

| Flag | Meaning |
|---|---|
| `--paper` | use the paper-trading endpoint |
| `--feed iex` | market-data feed if your plan has no SIP access |
| `--start YYYY-MM-DD` | analysis window start (default: start of the funded equity history) |
| `--combo-window 120` | seconds within which legs on one underlying form one strategy |
| `--rf 0.04` | risk-free rate for Sharpe/Sortino |
| `--offline` | embed plotly.js (≈4.5 MB) so the file opens without internet |

`data/` and `reports/` are git-ignored: your account data stays local.

## Sharing a report publicly (`--share`)

The normal report is a full record of your account. To publish one, build the shared view instead:

```bash
alpaca-analyzer report --data data/live --out reports/share.html --share --offline
```

| Kept as-is | Normalized | Removed |
|---|---|---|
| Returns, drawdowns, Sharpe/Sortino, β, win rates, return on risk, benchmark comparisons, strategy/DTE/outcome breakdowns, symbols traded, trade dates | Account curves become the growth of $10,000 (`--notional`); aggregate dollar charts are per $10,000 of average equity | Deposits/withdrawals, account size, open positions, payoff diagrams & Greeks, unrealized P&L by name, quantities, intraday times (and the entry-hour chart), per-trade prices, fees and dollar P&L |

Per-trade rows show percentages only. That's deliberate: a dollar amount next to a known contract price
and a 1-lot trade would let someone back out the normalization factor, and from it your account size.
Name, email, account number and API keys are never written to any report.

## Web app (GitHub Pages)

`web/` is a static page that runs the same analysis for anyone with Alpaca keys, entirely in their
browser: it calls Alpaca's API directly (GET requests only; Alpaca allows browser access) and runs
this Python package via [Pyodide](https://pyodide.org) (WebAssembly). Nothing is sent to any other server.
The first visit downloads ~20 MB of Python/pandas, which the browser then caches.

* **Keys**: held in memory by default (**Refresh** re-runs with them, no re-entry). "Remember keys on this
  device" stores them in `localStorage`; add a passphrase and they're stored AES-GCM-encrypted
  (PBKDF2-SHA256). Without one they're plain text, readable by any page on the same origin (all of a
  user's `*.github.io` project sites share one).
* **Keep unlocked (30 days)**: after one passphrase entry, the keys are also encrypted under a random,
  **non-extractable** WebCrypto key kept in IndexedDB, so the page opens straight into the report. The
  window slides with each visit; **Lock** or "Forget saved keys" ends it. While unlocked, same-origin
  scripts could ask the browser to decrypt, but the key bytes can't be read or copied out of the profile.
* **Data cache**: past activities, daily bars and equity history are kept in IndexedDB (encrypted under their
  own non-extractable key), so a refresh downloads only what's new. While the market is closed since the last
  download, a refresh checks just positions and open orders (2 requests), plus stock prices while an
  after-hours/overnight session trades; anything else comes from the saved copy. Bars are re-downloaded in
  full weekly and after splits. **Re-download everything** bypasses it; Lock and Forget delete it.
* **Isolation**: a Content-Security-Policy limits network access to Alpaca's hosts and the pinned CDN
  (scripts carry SRI hashes). The report renders in a sandboxed iframe with no access to the page's
  storage. Both the full and the shareable (`--share`) report can be downloaded.
* **Updates**: every build gets new file names (`app.js?v=<build>`, a wheel with the build in its name), so a
  browser reload never runs a stale cached copy. The open page also checks for a newer deployment on
  **Refresh** and when you return to the tab, and reloads into it (back to the same report tab when kept
  unlocked; otherwise it offers a reload button).
* **Deploy**: `.github/workflows/pages.yml` runs the tests, builds the site with
  `python scripts/build_site.py site`, and publishes it. One-time setup: repo **Settings → Pages →
  Source: GitHub Actions**.
* **Local preview**: `python scripts/build_site.py site && python -m http.server -d site 8000`.



* **Effective prices (cost ÷ quantity, fees included).** Every fee is folded into the fill it belongs
  to before lots are matched. Fees carrying an `order_id` (OCC clearing) go to that order's fills;
  daily regulatory fees go to that trade date's fills: TAF/REG to sells, ORF/OCC per option contract,
  CAT to every execution. Entry/exit prices, realized P&L, and open cost basis are all net of fees. The
  broker's gross price and average cost are kept for comparison.
* **FIFO lots** per symbol, long and short, with options at a ×100 multiplier.
* **Expirations / assignments / exercises** close the option at $0; the delivered stock (`OPTRD`) is
  booked at the strike, matching how Alpaca records it.
* **Splits** rescale open lots preserving basis; **spin-offs** are booked at Alpaca's reference price
  (flagged in the notes).
* **Returns** are time-weighted (Modified Dietz, daily, flows at the start of the day), so deposits never
  count as performance. Benchmarks are simulated with your exact deposits.
* **Buy & hold per symbol**: your peak capital committed to that name, invested in the underlying on
  your first trade date and held to today.
* **Reconciliation**: the ledger's open quantities are compared to Alpaca's positions; any mismatch
  (e.g. history that starts mid-position) is listed on the notes tab.

## Bundle format (offline / CSV import)

A bundle directory holds `activities.json` (raw Alpaca activities) and/or `activities.csv`,
`portfolio_history.json`, `positions.json`, `account.json`, `bars.json` (`{symbol: [{t, c}]}`),
and optionally `prices.json`, `option_snapshots.json`, `orders.json` (raw `/v2/orders?status=open&nested=true`), `stock_snapshots.json`
(`{feed: {symbol: snapshot}}`, for after-hours prints) and `clock.json` (raw `/v2/clock`). The CSV form is handy for hand-built or
exported histories:

```csv
activity_type,time,symbol,side,qty,price,net_amount,sub_type,order_id
FILL,2026-08-05T13:39:23Z,INTC260821C00102000,buy,2,6,,,
CSD,2026-08-21,,,,,1000,,
OPEXP,2026-08-21,PLTR260821C00185000,,-1,,0,,
FEE,2026-08-05,,,,,-0.05,OCC,<order id>
```

If no FEE rows are present, daily fee totals from `portfolio_history.json` are allocated instead.

## Development

```bash
pip install -e '.[test]'
```

```bash
pytest -q
```
