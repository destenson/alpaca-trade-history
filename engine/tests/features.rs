//! Feature tests ported from the retired Python suite: orders, re-pricing, fill odds, reinvest,
//! readings of open positions, the shared view and the Trades-tab buy & hold comparison.

use std::collections::HashMap;
use std::fs;

use alpaca_engine::analysis::{self, Analysis, ComboRow};
use alpaca_engine::fillodds::fill_odds;
use alpaca_engine::interpret::{build_units, name_group, readings, Greeks, OptRow, Unit, STOCK};
use alpaca_engine::occ::parse_occ;
use alpaca_engine::orders as O;
use alpaca_engine::reinvest::reinvest_table;
use alpaca_engine::reprice as rp;
use alpaca_engine::util::{date_ymd, from_ny, parse_ts, Ts};
use alpaca_engine::{build_reports, bundle_from_json, share};
use serde_json::{json, Value};

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * b.abs().max(1.0)
}

fn now() -> Ts {
    parse_ts("2026-11-22T17:31:09Z").unwrap()
}

fn fixture(name: &str) -> (Value, Value) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let b: Value = serde_json::from_str(&fs::read_to_string(format!("{dir}{name}.json")).unwrap()).unwrap();
    let o: Value = serde_json::from_str(&fs::read_to_string(format!("{dir}{name}.opts")).unwrap_or_else(|_| "{}".into())).unwrap();
    (b, o)
}

fn analyze(b: &Value, benchmarks: &[&str]) -> Analysis {
    let bm: Vec<String> = benchmarks.iter().map(|s| s.to_string()).collect();
    analysis::run(bundle_from_json(b), &bm, 120.0, None, 0.0)
}

fn reports(b: &Value, opts: &Value) -> Value {
    serde_json::from_str(&build_reports(&b.to_string(), &opts.to_string(), now(), 0).unwrap()).unwrap()
}

fn unit(sym: &str, qty: f64, cost: f64, value: f64) -> Unit {
    Unit { symbol: sym.into(), qty, cost, value, greeks: Greeks::default(), iv: None }
}

// ------------------------------------------------------------------------------------- orders
fn mleg(limit: f64, legs: &[(&str, &str, &str)], status: &str) -> Value {
    json!({"id": "x", "order_class": "mleg", "type": "limit", "qty": "1", "filled_qty": "0", "limit_price": limit.to_string(),
           "status": status, "time_in_force": "day", "submitted_at": "2026-11-22T00:07:42.812435195Z",
           "legs": legs.iter().map(|(s, side, intent)| json!({"symbol": s, "side": side, "position_intent": format!("{side}_to_{intent}"), "ratio_qty": "1"})).collect::<Vec<_>>()})
}

fn snap(bp: f64, ap: f64, iv: f64) -> Value {
    json!({"latestQuote": {"bp": bp, "ap": ap}, "impliedVolatility": iv})
}

fn evaluate(raw: Vec<Value>, snaps: Value, spot: f64) -> Vec<O::PendingOrder> {
    let a = analyze(&json!({"activities": [{"activity_type": "CSD", "date": "2026-01-05", "net_amount": "1000"}]}), &["SPY"]);
    let mut os = O::parse_orders(&raw);
    O::evaluate(&mut os, &a.ledger, &a.open_positions, &snaps, &|_| Some(spot), date_ymd(2026, 11, 22), None);
    os
}

#[test]
fn debit_vertical_at_limit_price() {
    let os = evaluate(
        vec![mleg(2.5, &[("AVGO261127P00317500", "buy", "open"), ("AVGO261127P00312500", "sell", "open")], "accepted")],
        json!({"AVGO261127P00317500": snap(16.3, 16.7, 0.5), "AVGO261127P00312500": snap(13.3, 13.8, 0.5)}),
        304.0,
    );
    let o = &os[0];
    assert_eq!((o.strategy.as_str(), o.effect.as_str()), ("Bear put spread", "open"));
    assert!(approx(o.max_gain, 250.0) && approx(o.max_loss, -250.0));
    assert_eq!(o.breakevens.len(), 1);
    assert!((o.breakevens[0] - 315.0).abs() < 0.05);
    assert!(approx(o.mid_now.unwrap(), 16.5 - 13.55) && approx(o.natural.unwrap(), 16.7 - 13.3));
    let pop = o.pop.unwrap();
    assert!(0.5 < pop && pop < 1.0);
}

#[test]
fn negative_limit_is_a_credit() {
    let os = evaluate(vec![mleg(-1.2, &[("X261127P00100000", "sell", "open"), ("X261127P00095000", "buy", "open")], "accepted")], json!({}), 105.0);
    let o = &os[0];
    assert_eq!(o.strategy, "Bull put spread");
    assert!(approx(-o.net.unwrap() * o.units * o.mult(), 120.0));
    assert!(approx(o.max_gain, 120.0) && approx(o.max_loss, -380.0));
}

#[test]
fn closing_and_done_orders_are_not_projected() {
    let raw = vec![
        mleg(-1.65, &[("INTC261127C00051000", "buy", "close"), ("INTC261127C00049000", "sell", "close")], "accepted"),
        mleg(1.0, &[("X261127C00100000", "buy", "open"), ("X261127C00105000", "sell", "open")], "filled"),
        json!({"id": "s", "order_class": "simple", "type": "limit", "symbol": "PACB", "side": "sell", "qty": "100", "filled_qty": "0",
               "limit_price": "16", "status": "new", "time_in_force": "gtc"}),
    ];
    let os = evaluate(raw, json!({}), 50.0);
    assert_eq!(os.len(), 2);
    assert!(os[0].effect == "close" && os[0].max_loss.is_nan());
    assert!(!os[1].is_option);
}

#[test]
fn single_leg_option_and_partial_fill() {
    let raw = vec![json!({"id": "a", "order_class": "simple", "type": "limit", "symbol": "AURA270416C00006000", "side": "buy", "qty": "3",
                          "filled_qty": "1", "limit_price": "0.10", "status": "partially_filled", "time_in_force": "gtc"})];
    let os = evaluate(raw, json!({"AURA270416C00006000": snap(0.08, 4.8, 1.2)}), 6.8);
    let o = &os[0];
    assert!(o.units == 2.0 && o.strategy == "Long call");
    assert!(o.max_gain.is_infinite() && approx(o.max_loss, -20.0));
}

#[test]
fn orders_on_contracts_with_no_quotes_yet() {
    // a $0 inverted strangle on contracts that haven't traded: payoff kept, estimates and fill odds not guessed
    let (mut b, _) = fixture("08_order_estimate_and_report_inputs");
    b["orders"] = json!([{"id": "ant", "order_class": "mleg", "type": "limit", "qty": "10", "filled_qty": "0", "limit_price": "0",
        "status": "accepted", "time_in_force": "gtc", "submitted_at": "2026-11-22T03:26:01Z",
        "legs": [{"symbol": "ZS261218C00200000", "side": "buy", "position_intent": "buy_to_open", "ratio_qty": "1"},
                 {"symbol": "ZS261218P00230000", "side": "buy", "position_intent": "buy_to_open", "ratio_qty": "1"}]}]);
    let (o, _) = zs_order(&b);
    assert!(o.no_market && o.est_open.is_none() && o.model.is_none() && o.pop.is_none());
    assert!(approx(o.max_loss, 30.0 * 100.0 * 10.0));          // worst case at expiration is still a $30/share gain
    let r = reports(&b, &json!({}));
    let full = r["full"].as_str().unwrap();
    for s in ["data-tip=\"No quotes yet: these contracts haven&#x27;t traded. Nothing to estimate", "no quotes yet</td>", "1 on contracts with no quotes yet"] {
        assert!(full.contains(s), "{s}");
    }
}

fn zigzag_bars(days: &[&str]) -> Value {
    // 78 regular-session 5-minute closes a day alternating 100 / 101, plus an after-hours print that is ignored
    let mut v = vec![];
    for d in days {
        for k in 0..78 {
            let m = 14 * 60 + 30 + 5 * k;
            v.push(json!({"t": format!("{d}T{:02}:{:02}:00Z", m / 60, m % 60), "c": if k % 2 == 0 { 100.0 } else { 101.0 }}));
        }
        v.push(json!({"t": format!("{d}T22:00:00Z"), "c": 150.0}));
    }
    Value::Array(v)
}

#[test]
fn realized_vol_from_intraday_bars() {
    let bars = zigzag_bars(&["2026-11-19", "2026-11-20"]);
    // 155 returns of ln(1.01) (77 a session + the overnight one) over 2 days, annualized with 252 days
    let want = (1.01f64.ln().powi(2) * 155.0 / 2.0 * 252.0).sqrt();
    assert!(approx(rp::realized_vol(&bars).unwrap(), want));
    assert!(rp::realized_vol(&json!([{"t": "2026-11-19T15:00:00Z", "c": 1.0}])).is_none());
}

#[test]
fn unquoted_contracts_priced_from_realized_vol() {
    // same $0 strangle, but nothing on the chain has a quote or an IV: price it from the stock's own volatility
    let (mut b, _) = fixture("08_order_estimate_and_report_inputs");
    b["option_snapshots"] = json!({});
    b["intraday"] = json!({"ZS": zigzag_bars(&["2026-11-19", "2026-11-20"])});
    b["orders"] = json!([{"id": "ant", "order_class": "mleg", "type": "limit", "qty": "10", "filled_qty": "0", "limit_price": "0",
        "status": "accepted", "time_in_force": "gtc", "submitted_at": "2026-11-22T03:26:01Z",
        "legs": [{"symbol": "ZS261218C00200000", "side": "buy", "position_intent": "buy_to_open", "ratio_qty": "1"},
                 {"symbol": "ZS261218P00230000", "side": "buy", "position_intent": "buy_to_open", "ratio_qty": "1"}]}]);
    let r = reports(&b, &json!({}));
    let full = r["full"].as_str().unwrap();
    assert!(full.contains("Estimates use ZS&#x27;s own 139% realized volatility"), "tooltip");
    assert!(!full.contains("no quotes yet</td>"), "fill odds are simulated now");
    for s in [">Fill</th>", ">Exp. profit</th>", "Expected fills"] {
        assert!(full.contains(s), "{s}");
    }
}

#[test]
fn fill_odds_include_profit_if_filled() {
    let b = zs_bundle(2.0, "gtc");
    let (o, m) = zs_order(&b);
    let f = fill_odds(&o, &m, 0.0).unwrap();
    assert!(approx(f.p_fill, f.expiry.unwrap()));
    assert!(f.fill_and_profit <= f.p_fill + 1e-12);
    if let Some(p) = f.profit_if_filled {
        assert!(approx(p * f.p_fill, f.fill_and_profit));
    }
}

#[test]
fn orders_tab_and_shared_view() {
    let (b, o) = fixture("07_order_symbols_and_report_tab");
    let r = reports(&b, &o);
    let full = r["full"].as_str().unwrap();
    assert!(full.contains("id=\"orders\"") && full.contains("ZS: all 2 orders together"));
    assert!(!r["share"].as_str().unwrap().contains("id=\"orders\""));
}

// ---------------------------------------------------------------------------------- re-pricing
fn clock_closed() -> Value {
    json!({"is_open": false, "next_open": "2026-11-23T09:30:00-05:00", "timestamp": "2026-11-22T12:31:09.208776118-05:00"})
}

fn stock(close: f64, last: f64, last_t: &str, conds: &[&str]) -> Value {
    json!({"dailyBar": {"c": close, "t": "2026-11-20T05:00:00Z"}, "latestTrade": {"p": last, "t": last_t, "s": 100, "c": conds}})
}

fn market(clock: &Value, snaps: &Value, syms: &[&str], qt: HashMap<String, Ts>) -> rp::Market {
    let s: Vec<String> = syms.iter().map(|x| x.to_string()).collect();
    rp::market_state(Some(clock), Some(snaps), &s, &qt, &HashMap::new(), &now())
}

#[test]
fn market_state_uses_after_hours_print_and_next_open() {
    let snaps = json!({"default": {"ZS": stock(194.83, 226.15, "2026-11-20T21:35:15.469443849Z", &["@", "T"]),
                                   "ADI": stock(346.55, 346.55, "2026-11-20T20:59:58Z", &["@"])},
                       "overnight": {"ZS": stock(199.97, 199.97, "2026-11-20T08:02:09Z", &["@"])}});
    let m = market(&clock_closed(), &snaps, &["ZS", "ADI"], HashMap::new());
    let (zs, adi) = (&m.underlyings["ZS"], &m.underlyings["ADI"]);
    assert_eq!((zs.refp, zs.spot, zs.last_ext, zs.stale()), (Some(194.83), Some(226.15), true, true));
    assert!(approx(zs.mv(), 226.15 / 194.83 - 1.0));
    assert_eq!((adi.spot, adi.stale()), (Some(346.55), false));
    assert_eq!(m.valuation, parse_ts("2026-11-23T09:30:00-05:00").unwrap());
}

#[test]
fn newer_overnight_print_wins() {
    let snaps = json!({"default": {"X": stock(100.0, 103.0, "2026-11-20T21:30:00Z", &["@", "T"])},
                       "overnight": {"X": stock(101.0, 108.0, "2026-11-23T03:00:00Z", &["@"])}});
    let m = market(&clock_closed(), &snaps, &["X"], HashMap::new());
    let u = &m.underlyings["X"];
    assert_eq!((u.spot, u.last_feed.as_str(), u.last_ext), (Some(108.0), "overnight", true));
}

#[test]
fn market_open_means_no_repricing() {
    let snaps = json!({"default": {"X": stock(100.0, 103.0, "2026-11-23T15:00:00Z", &["@"])}});
    let m = market(&json!({"is_open": true, "timestamp": "2026-11-23T10:00:00-05:00"}), &snaps, &["X"], HashMap::new());
    let u = &m.underlyings["X"];
    assert!(u.refp == Some(103.0) && u.spot == Some(103.0) && !u.stale());
}

fn opt_snap(sym: &str, s: f64, iv: f64, t_q: &Ts) -> Value {
    let c = parse_occ(sym).unwrap();
    let mid = rp::bs(s, c.strike, rp::years(t_q, c.expiry), iv, c.right);
    let r4 = |x: f64| (x * 1e4).round() / 1e4;
    json!({"latestQuote": {"bp": r4(mid * 0.98), "ap": r4(mid * 1.02), "t": "2026-11-20T20:59:59.5Z"}})
}

#[test]
fn calibration_round_trips_the_quote_then_reprices() {
    let t_q = parse_ts("2026-11-20T20:59:59.5Z").unwrap();
    let sym = "ZS261127C00220000";
    let sn = opt_snap(sym, 194.83, 0.7, &t_q);
    let m = market(&clock_closed(), &json!({"default": {"ZS": stock(194.83, 226.15, "2026-11-20T21:35:15Z", &["@", "T"])}}), &["ZS"],
                   HashMap::from([("ZS".to_string(), t_q)]));
    let lm = rp::calibrate(sym, &sn, &m.underlyings["ZS"], &m, None).unwrap();
    assert_eq!(lm.iv_source, "quote mid");
    assert!((lm.iv - 0.7).abs() < 0.7e-3);
    let mid = (sn["latestQuote"]["bp"].as_f64().unwrap() + sn["latestQuote"]["ap"].as_f64().unwrap()) / 2.0;
    let back = rp::bs(194.83, 220.0, rp::years(&t_q, date_ymd(2026, 11, 27)), lm.iv, 'C');
    assert!((back - mid).abs() < 1e-3 * mid);
    assert!(lm.value(226.15, 0.0) > 6.0 && 6.0 > mid);
    assert!(lm.value(194.83, 0.0) < mid);
    assert!(lm.value(226.15, -0.4) < lm.value(226.15, 0.0));
}

#[test]
fn unusable_quotes_fall_back() {
    let m = market(&clock_closed(), &json!({"default": {"A": stock(6.875, 6.875, "2026-11-20T20:59:00Z", &["@"])}}), &["A"], HashMap::new());
    let wide = json!({"latestQuote": {"bp": 0.08, "ap": 4.81, "t": "2026-11-20T20:59:59.5Z"}, "impliedVolatility": 1.28});
    let lm = rp::calibrate("A270416C00006000", &wide, &m.underlyings["A"], &m, None).unwrap();
    assert_eq!((lm.iv_source, lm.iv), ("Alpaca IV", 1.28));
    let lm = rp::calibrate("A270416C00006000", &json!({}), &m.underlyings["A"], &m, Some(0.9)).unwrap();
    assert_eq!((lm.iv_source, lm.iv), ("underlying avg", 0.9));
}

#[test]
fn trading_day_clock_skips_weekends_and_holidays() {
    let fri_close = from_ny(date_ymd(2026, 11, 20), 16, 0, 0);
    let mon_open = from_ny(date_ymd(2026, 11, 23), 9, 30, 0);
    assert!(approx(rp::trading_days(&fri_close, &mon_open), 8.0 / 24.0 + 9.5 / 24.0));
    assert!(approx(rp::trading_days(&mon_open, &rp::expiry_time(date_ymd(2026, 11, 27))), 14.5 / 24.0 + 2.0 + 16.0 / 24.0));
    assert!(rp::nyse_holidays(2026).contains(&date_ymd(2026, 4, 3)));
    assert!(rp::nyse_holidays(2027).contains(&date_ymd(2027, 6, 18)));
    assert!(!rp::nyse_holidays(2027).contains(&date_ymd(2027, 12, 31)));
    let n = rp::next_open(&from_ny(date_ymd(2026, 11, 25), 17, 0, 0));
    assert_eq!(alpaca_engine::util::ny_date(&n), date_ymd(2026, 11, 27));
}

/// The bundle of the re-pricing tests (a ZS call spread order, market closed, ZS up after hours).
fn zs_bundle(limit: f64, tif: &str) -> Value {
    let (mut b, _) = fixture("08_order_estimate_and_report_inputs");
    b["orders"][0]["limit_price"] = json!(limit.to_string());
    b["orders"][0]["time_in_force"] = json!(tif);
    b
}

fn zs_order(b: &Value) -> (O::PendingOrder, rp::Market) {
    let a = analyze(b, &["SPY", "QQQ"]);
    let qt = rp::quote_times(&b["option_snapshots"]);
    let m = rp::market_state(Some(&b["clock"]), Some(&b["stock_snapshots"]), &["ZS".to_string()], &qt, &HashMap::new(), &now());
    let mut os = O::parse_orders(b["orders"].as_array().unwrap());
    O::evaluate(&mut os, &a.ledger, &a.open_positions, &b["option_snapshots"], &|_| None, date_ymd(2026, 11, 22), Some(&m));
    (os.remove(0), m)
}

#[test]
fn order_estimate_and_report_inputs() {
    let b = zs_bundle(0.34, "day");
    let (o, _) = zs_order(&b);
    assert_eq!(o.spot, Some(226.15));
    assert!(approx(o.mid_now.unwrap(), o.legs[0].mid().unwrap() - o.legs[1].mid().unwrap()));
    let e = o.est_open.unwrap();
    assert!(2.0 < e && e < 5.0, "{e}");
    let r = reports(&b, &json!({}));
    let full = r["full"].as_str().unwrap();
    for s in ["id=\"rp-data\"", "class=\"rp-spot\"", "Est. at open", "Moved since quotes"] {
        assert!(full.contains(s), "{s}");
    }
}

#[test]
fn open_position_card_is_repriced() {
    let (b, o) = fixture("09_open_position_card_is_repriced");
    let r = reports(&b, &o);
    let full = r["full"].as_str().unwrap();
    assert!(full.contains("id=\"est_rd_0\"") && full.contains("Whole position at next open (est.)"));
    let i = full.find("id=\"rp-data\">").unwrap() + "id=\"rp-data\">".len();
    let j = i + full[i..].find("</script>").unwrap();
    let payload: Value = serde_json::from_str(&full[i..j].replace("<\\/", "</")).unwrap();
    assert!(payload["views"].as_array().unwrap().iter().any(|v| v["kind"] == "pos" && v["und"] == "ZS"));
}

#[test]
fn fill_odds_bounds_and_monotonic() {
    let (o, m) = zs_order(&zs_bundle(4.9, "day"));
    assert_eq!(fill_odds(&o, &m, 0.0).unwrap().h[0], Some(1.0));
    let (o, m) = zs_order(&zs_bundle(0.01, "day"));
    let f = fill_odds(&o, &m, 0.0).unwrap();
    assert!(f.h[0].unwrap() < 0.02 && f.h[1].is_none() && f.expiry.is_none());
    let lim = (o.est_open.unwrap() * 0.85 * 100.0).round() / 100.0;
    let (o, m) = zs_order(&zs_bundle(lim, "gtc"));
    let f = fill_odds(&o, &m, 0.0).unwrap();
    let seq = [f.h[0].unwrap(), f.h[1].unwrap(), f.h[2].unwrap(), f.h[3].unwrap(), f.expiry.unwrap()];
    assert!(seq.windows(2).all(|w| w[0] <= w[1] + 1e-12), "{seq:?}");
    assert!(0.0 < seq[4] && seq[4] < 1.0);
}

// ----------------------------------------------------------------------- trades tab: buy & hold
fn sym<'a>(a: &'a Analysis, u: &str) -> &'a analysis::SymRow {
    a.symbols.iter().find(|r| r.underlying == u).unwrap()
}

#[test]
fn trades_tab_is_closed_positions_only() {
    let (b, o) = fixture("10_trades_tab_is_closed_positions_only");
    let a = analyze(&b, &["SPY", "QQQ"]);
    let s = sym(&a, "AAA");
    assert!(approx(s.realized, 20.0) && !s.is_open);
    assert_eq!(s.bhc_end, Some(date_ymd(2026, 1, 8)));
    let r = reports(&b, &o);
    let full = r["full"].as_str().unwrap();
    let i = full.find("const FIGS = ").unwrap() + 13;
    let j = i + full[i..].find(";\n").unwrap();
    let figs: Value = serde_json::from_str(&full[i..j].replace("<\\/", "</")).unwrap();
    let d = &figs["f_bysym"]["data"][0];
    assert_eq!(d["y"], json!(["AAA"]));
    assert_eq!(d["x"], json!([20.0]));
}

#[test]
fn buy_and_hold_counts_partially_closed_names() {
    let (b, _) = fixture("11_buy_and_hold_counts_partially_closed_names");
    let a = analyze(&b, &["SPY", "QQQ"]);
    let s = sym(&a, "CCC");
    assert!(s.is_open && approx(s.realized, 10.0));
    assert_eq!(s.bhc_end, Some(date_ymd(2026, 1, 8)));
    assert!(approx(s.bhc_capital.unwrap(), 100.0) && approx(s.bhc_pnl.unwrap(), 100.0 * (12.0 / 10.0 - 1.0)));
}

#[test]
fn buy_and_hold_for_options_only_name_uses_bars() {
    let (b, _) = fixture("12_buy_and_hold_for_options_only_name_uses_bars");
    let a = analyze(&b, &["SPY", "QQQ"]);
    let s = sym(&a, "DDD");
    assert!(approx(s.realized, 50.0));
    assert_eq!(s.bhc_end, Some(date_ymd(2026, 1, 9)));
    assert!(approx(s.bhc_capital.unwrap(), 200.0) && approx(s.bhc_pnl.unwrap(), 200.0 * (55.0 / 50.0 - 1.0)));
}

#[test]
fn short_history_raises_alert_not_flat_chart() {
    let (b, _) = fixture("02_short_history_raises_alert_not_flat_chart");
    let a = analyze(&b, &["SPY"]);
    let text = a.alerts.join(" ");
    assert!(text.contains("Portfolio history ends 2026-01-30"), "{text}");
    assert!(text.contains("$0") && text.contains("could not be counted"));
    assert_eq!(*a.equity.as_ref().unwrap().dates.last().unwrap(), date_ymd(2026, 1, 30));
}

// ----------------------------------------------------------------------------------- reinvest
fn combo(und: &str, close: &str, pnl: f64) -> ComboRow {
    let t = parse_ts(close).unwrap();
    ComboRow {
        combo_id: 1, underlying: und.into(), strategy: "Long call".into(), open_time: t, close_time: Some(t), n_legs: 1, legs: String::new(),
        net_premium: 0.0, premium_side: "Debit", max_risk: f64::NAN, dte: 0, realized_pnl: pnl, unrealized_pnl: 0.0, pnl, outcome: "Closed early",
        is_open: false, rolled_from: None, rolled_to: None, underlying_px: f64::NAN, moneyness: f64::NAN, rights: "C".into(), hold_days: 0.0,
        ror: f64::NAN, campaign: 1,
    }
}

fn xyz_bars() -> Value {
    json!({"XYZ": [{"t": "2026-01-05T05:00:00Z", "c": 40.0}, {"t": "2026-01-06T05:00:00Z", "c": 50.0},
                   {"t": "2026-01-07T05:00:00Z", "c": 25.0}, {"t": "2026-01-08T05:00:00Z", "c": 30.0}]})
}

fn xyz_combos() -> Vec<ComboRow> {
    vec![combo("XYZ", "2026-01-05T20:00:00Z", 90.0), combo("XYZ", "2026-01-06T20:00:00Z", -60.0), combo("XYZ", "2026-01-07T20:00:00Z", 20.0)]
}

#[test]
fn reinvest_wins_only_buys_whole_shares_and_carries_the_rest() {
    let held = indexmap::IndexMap::from([("XYZ".to_string(), 1.0)]);
    let r = &reinvest_table(&xyz_combos(), &xyz_bars(), &|_| Some(30.0), &held)[0];
    assert_eq!((r.wins, r.losses, r.profits), (2, 1, 110.0));
    assert!(r.shares == 3 && approx(r.cost, 105.0) && approx(r.cash, 5.0));
    assert!(approx(r.value, 90.0) && approx(r.gain, -15.0));
    assert_eq!(r.short_by, 2.0);
}

#[test]
fn reinvest_with_losses_must_earn_losses_back() {
    let r = &reinvest_table(&xyz_combos(), &xyz_bars(), &|_| Some(30.0), &indexmap::IndexMap::new())[0];
    assert!(r.net_shares == 2 && approx(r.net_cash, -30.0));
}

#[test]
fn reinvest_closing_day_without_price_history_buys_nothing() {
    let r = &reinvest_table(&[combo("ABC", "2026-01-05T20:00:00Z", 500.0)], &json!({}), &|_| None, &indexmap::IndexMap::new())[0];
    assert!(r.shares == 0 && r.cash == 500.0 && r.unpriced == 1 && r.value.is_nan());
}

// ------------------------------------------------------------------------- readings (interpret)
const LEGS: [(&str, f64, f64); 6] = [
    ("X261123C00130000", 1.0, 385.0),
    ("X261123C00131000", -1.0, -335.0),
    ("X261125C00136000", -1.0, -485.0),
    ("X261125C00137000", 1.0, 432.0),
    ("X261125P00134000", -1.0, -397.0),
    ("X261125P00136000", 1.0, 487.0),
];

fn six_leg() -> (Vec<alpaca_engine::interpret::Reading>, alpaca_engine::interpret::Whole) {
    let units: Vec<Unit> = LEGS.iter().map(|(s, q, c)| unit(s, *q, *c, 0.0)).collect();
    let opened = vec![
        vec!["X261123C00130000".to_string(), "X261123C00131000".to_string()],
        ["X261125C00136000", "X261125C00137000", "X261125P00134000", "X261125P00136000"].iter().map(|s| s.to_string()).collect(),
    ];
    readings(&units, 128.0, date_ymd(2026, 11, 20), Some(&opened))
}

#[test]
fn as_opened_first_and_alternatives_present() {
    let (rs, _) = six_leg();
    assert!(rs[0].as_opened && rs[0].label() == "Bull call spread + Bearish double vertical");
    let labels: Vec<String> = rs.iter().map(|r| r.label()).collect();
    assert!(labels.iter().any(|l| l == "Bull call spread + Bear call spread + Bear put spread"));
    assert!(labels.iter().any(|l| l.contains("Diagonal spread")));
    let uniq: std::collections::HashSet<&String> = labels.iter().collect();
    assert_eq!(uniq.len(), labels.len());
}

#[test]
fn same_legs_same_total_but_different_grouped_risk() {
    let (rs, whole) = six_leg();
    for r in &rs {
        assert!((r.groups.iter().map(|g| g.unrealized()).sum::<f64>() - whole.unrealized).abs() < 1e-6);
    }
    assert!(rs[0].undefined() == 0 && rs[0].sum_max_loss() == -187.0);
    let naked = rs.iter().find(|r| r.undefined() > 0).unwrap();
    assert!(naked.groups.iter().any(|g| g.max_loss.is_infinite()));
}

#[test]
fn stock_lots_pair_with_options() {
    let s = unit(STOCK, 100.0, 5000.0, 5000.0);
    assert_eq!(name_group(&[&s, &unit("X261218C00055000", -1.0, -150.0, -150.0)]), "Covered call");
    assert_eq!(name_group(&[&s, &unit("X261218P00045000", 1.0, 90.0, 90.0)]), "Protective put");
    let rows = [OptRow { symbol: "X261218C00055000".into(), qty: -2.0, cost_basis: -300.0, market_value: -250.0 }];
    let (units, odd, split) = build_units(&rows, 250.0, 12500.0, 12000.0, &json!({}));
    assert!(split && units.iter().filter(|u| u.symbol == STOCK).count() == 2 && odd.unwrap().qty == 50.0);
    let (rs, whole) = readings(&units, 50.0, date_ymd(2026, 11, 20), None);
    assert!(rs.iter().any(|r| r.label() == "Covered call + Covered call"));
    for r in &rs {
        let sh: f64 = r.groups.iter().flat_map(|g| &g.units).filter(|u| u.symbol == STOCK).map(|u| u.qty).sum();
        assert_eq!(sh, 200.0);
    }
    assert!(approx(whole.max_gain, 2.0 * 100.0 * 55.0 - 2.0 * 100.0 * 50.0 + 300.0));
}

#[test]
fn round_lots_kept_when_too_many_units_to_split() {
    let rows = [OptRow { symbol: "X261218C00055000".into(), qty: -9.0, cost_basis: -900.0, market_value: -800.0 }];
    let (units, odd, split) = build_units(&rows, 950.0, 47500.0, 47000.0, &json!({}));
    let st: Vec<f64> = units.iter().filter(|u| u.symbol == STOCK).map(|u| u.qty).collect();
    assert!(!split && st == vec![900.0] && odd.unwrap().qty == 50.0);
}

// --------------------------------------------------------------------------------- shared view
#[test]
fn strip_leg_qty() {
    assert_eq!(share::strip_leg_qty("+3 10/16 1C, -1 11/20 3C"), "+ 10/16 1C, − 11/20 3C");
    assert_eq!(share::strip_leg_qty("+1 11/23 130C"), "+ 11/23 130C");
}

#[test]
fn share_report_has_no_account_details() {
    let (b, o) = fixture("16_share_report_has_no_account_details");
    let r = reports(&b, &o);
    let html = r["share"].as_str().unwrap();
    let i = html.find("const FIGS = ").unwrap() + 13;
    let j = i + html[i..].find(";\n").unwrap();
    let figs = &html[i..j];
    let mut blob = String::new();
    let mut rest = html;
    while let Some(k) = rest.find("<script") {
        blob.push_str(&rest[..k]);
        rest = rest[k..].find("</script>").map(|e| &rest[k + e + 9..]).unwrap_or("");
    }
    blob.push_str(rest);
    blob.push_str(figs);
    for secret in ["73,519", "73519", "4,321", "4321", "78,911", "78911", "78,966", "78966", "×37", "×7", "4,400", "4400"] {
        assert!(!blob.contains(secret), "{secret}");
    }
    let intraday = figs.as_bytes().windows(16).any(|w| {
        let s = String::from_utf8_lossy(w);
        s.len() == 16 && s.chars().enumerate().all(|(k, c)| match k {
            4 | 7 => c == '-',
            10 => c == ' ',
            13 => c == ':',
            _ => c.is_ascii_digit(),
        })
    });
    assert!(!intraday, "intraday times in shared charts");
    assert!(!html.contains(">Net premium<") && !html.contains(">Max risk<"));
    assert!(!html.contains("data-tab=\"positions\"") && !html.contains("Open option Greeks"));
    let fv: Value = serde_json::from_str(&figs.replace("<\\/", "</")).unwrap();
    assert!(!fv["f_bysym"]["data"].to_string().contains("\"QQQ\""));
    let a = analyze(&b, &["SPY"]);
    let s = share::make_shareable(&a, 10_000.0);
    let (sa, ss) = (a.stats.as_ref().unwrap(), s.stats.as_ref().unwrap());
    assert!(approx(ss.total_return, sa.total_return) && approx(ss.max_dd, sa.max_dd));
    assert!(approx(ss.ending_equity, 10_000.0 * a.equity.as_ref().unwrap().twr.last().unwrap()));
}

#[test]
fn option_strikes_adjusted_for_a_special_dividend() {
    // COST $30 special dividend: the 1100P/1095P spread becomes 1070P/1065P. Alpaca books each contract out
    // and its replacement in at the same price (OPCA, one corporate action id); the lots carry over.
    let fill = |sym: &str, side: &str, px: f64, t: &str| json!({"activity_type": "FILL", "id": format!("f{sym}"), "symbol": sym, "side": side, "qty": "1", "price": px.to_string(), "transaction_time": t, "order_id": "o1", "type": "fill"});
    let opca = |sym: &str, qty: i64, px: f64, id: &str| json!({"activity_type": "OPCA", "activity_sub_type": "DIV.CDIV", "id": id, "date": "2026-11-24", "created_at": "2026-11-24T07:40:49Z",
        "symbol": sym, "qty": qty.to_string(), "price": px.to_string(), "net_amount": (-(qty as f64) * px * 100.0).to_string(), "corporate_action_id": "ca1"});
    let pos = |sym: &str, qty: i64| json!({"symbol": sym, "asset_class": "us_option", "qty": qty.to_string(), "avg_entry_price": "1", "market_value": "100", "cost_basis": "100", "current_price": "1"});
    let b = json!({
        "activities": [
            {"activity_type": "CSD", "id": "d1", "date": "2026-11-01", "net_amount": "5000"},
            fill("COST261127P01100000", "buy", 10.75, "2026-11-20T15:00:00Z"),
            fill("COST261127P01095000", "sell", 8.58, "2026-11-20T15:00:00Z"),
            opca("COST261127P01100000", -1, 10.75, "a1"), opca("COST261127P01070000", 1, 10.75, "a2"),
            opca("COST261127P01095000", 1, 8.58, "a3"), opca("COST261127P01065000", -1, 8.58, "a4"),
        ],
        "positions": [pos("COST261127P01070000", 1), pos("COST261127P01065000", -1)],
        "bars": {}, "orders": [], "option_snapshots": {}, "stock_snapshots": {},
    });
    let a = analyze(&b, &[]);
    let held: HashMap<String, (f64, f64)> = a.ledger.open_lots.iter().filter(|(_, l)| !l.is_empty())
        .map(|(s, l)| (s.clone(), (l.iter().map(|x| x.qty).sum(), l[0].price))).collect();
    assert_eq!(held.len(), 2, "{held:?}");
    assert_eq!(held["COST261127P01070000"].0, 1.0);
    assert!((held["COST261127P01070000"].1 - 10.75).abs() < 0.05);
    assert_eq!(held["COST261127P01065000"].0, -1.0);
    match a.reconciliation {
        Some(analysis::Recon::Rows(r)) => assert!(r.is_empty(), "{r:?}"),
        _ => panic!("no reconciliation"),
    }
    // the strategy's legs follow the new strikes, and nothing was realized
    assert!(a.combos.iter().any(|c| c.legs.contains("1070P") && c.legs.contains("1065P")), "{:?}", a.combos.iter().map(|c| &c.legs).collect::<Vec<_>>());
    assert!(a.ledger.closed_df().is_empty());
}

#[test]
fn expiry_payoff_is_solved_exactly() {
    use alpaca_engine::optmath::{exact_expiry, LegIn};
    let leg = |sym: &str, qty: f64, cost: f64| LegIn { symbol: sym.into(), qty, cost, iv: None };
    // inverted iron condor for a $2.00 credit: profit below 47 and above 50, exactly $0 in between
    let (be, z, mx, mn) = exact_expiry(&[leg("CRWV261211C00046000", -1.0, -300.0), leg("CRWV261211C00047000", 1.0, 50.0),
        leg("CRWV261211P00050000", 1.0, 50.0), leg("CRWV261211P00051000", -1.0, -0.0)], 0.0, 0.0);
    assert!(be.is_empty(), "{be:?}");
    assert_eq!(z, vec![(f64::NEG_INFINITY, 47.0), (50.0, f64::INFINITY)]);
    assert!((mx - 100.0).abs() < 1e-9 && mn.abs() < 1e-9, "{mx} {mn}");
    // the 83/84 call + 84.5/85 put debit pair at $0.05: profits everywhere, 4 corners, 45 to 145
    let (be, z, mx, mn) = exact_expiry(&[leg("ANT261204C00083000", 1.0, 5.0), leg("ANT261204C00084000", -1.0, 0.0),
        leg("ANT261204P00085000", 1.0, 0.0), leg("ANT261204P00084500", -1.0, 0.0)], 0.0, 0.0);
    assert!(be.is_empty());
    assert_eq!(z, vec![(f64::NEG_INFINITY, f64::INFINITY)]);
    assert!((mx - 145.0).abs() < 1e-9 && (mn - 45.0).abs() < 1e-9, "{mx} {mn}");
    // a long call: one breakeven at strike + premium, unlimited upside
    let (be, z, mx, mn) = exact_expiry(&[leg("AURA270416C00007000", 1.0, 5.0)], 0.0, 0.0);
    assert_eq!(be, vec![7.05]);
    assert_eq!(z, vec![(7.05, f64::INFINITY)]);
    assert!(mx.is_infinite() && (mn + 5.0).abs() < 1e-9);
}
