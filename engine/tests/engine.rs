//! Behaviour tests (ported from the Python suite) plus a smoke test over every fixture bundle.

use std::collections::BTreeMap;
use std::fs;

use alpaca_engine::ledger::{allocate_fees, build_ledger, normalize, Ledger};
use alpaca_engine::util::{date_ymd, parse_ts};
use serde_json::{json, Value};

fn fill(t: &str, sym: &str, side: &str, qty: f64, px: f64, oid: Option<&str>) -> Value {
    json!({"activity_type": "FILL", "transaction_time": t, "symbol": sym, "side": side, "qty": qty.to_string(), "price": px.to_string(), "order_id": oid})
}

fn ledger_of(acts: Vec<Value>) -> Ledger {
    let mut evs = normalize(&acts);
    allocate_fees(&mut evs, &BTreeMap::new());
    build_ledger(evs, 120.0)
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6 * b.abs().max(1.0)
}

#[test]
fn fifo_partial_and_flip() {
    let l = ledger_of(vec![
        fill("2026-01-05T15:00:00Z", "XYZ", "buy", 10.0, 10.0, None),
        fill("2026-01-06T15:00:00Z", "XYZ", "buy", 10.0, 12.0, None),
        fill("2026-01-07T15:00:00Z", "XYZ", "sell", 15.0, 13.0, None),
    ]);
    let pnl: Vec<f64> = l.closed.iter().map(|c| c.pnl()).collect();
    assert!(approx(pnl[0], 30.0) && approx(pnl[1], 5.0));
    let lot = &l.open_lots["XYZ"][0];
    assert!(approx(lot.qty, 5.0) && approx(lot.price, 12.0));
}

#[test]
fn fees_fold_into_effective_price() {
    let l = ledger_of(vec![
        fill("2026-01-05T15:00:00Z", "ABC260116C00010000", "buy", 2.0, 1.0, Some("o1")),
        fill("2026-01-06T15:00:00Z", "ABC260116C00010000", "sell", 2.0, 1.5, Some("o2")),
        json!({"activity_type": "FEE", "activity_sub_type": "OCC", "order_id": "o1", "date": "2026-01-05", "net_amount": "-0.06"}),
        json!({"activity_type": "FEE", "activity_sub_type": "TAF", "date": "2026-01-06", "net_amount": "-0.02"}),
    ]);
    assert_eq!(l.closed.len(), 1);
    let c = &l.closed[0];
    assert!(approx(c.open_price, 1.0003));
    assert!(approx(c.close_price, 1.4999));
    assert!(approx(c.pnl(), 100.0 - 0.08));
}

#[test]
fn expiration_assignment_and_covered_call() {
    let l = ledger_of(vec![
        fill("2026-08-03T14:00:00Z", "RXT", "buy", 100.0, 4.5, None),
        fill("2026-08-03T15:00:00Z", "RXT260807C00004000", "sell", 1.0, 0.75, None),
        json!({"activity_type": "OPASN", "date": "2026-08-07", "symbol": "RXT260807C00004000", "qty": "1", "net_amount": "0"}),
        json!({"activity_type": "OPTRD", "date": "2026-08-07", "symbol": "RXT", "qty": "-100", "price": "4", "net_amount": "400"}),
    ]);
    assert_eq!(l.combos[0].strategy, "Covered call");
    let opt = l.closed.iter().find(|c| c.is_option).unwrap();
    assert_eq!(opt.close_kind, "assigned");
    assert!(approx(opt.pnl(), 75.0));
    let stk = l.closed.iter().find(|c| !c.is_option).unwrap();
    assert!(approx(stk.pnl(), -50.0));
    assert!(l.open_lots.is_empty());
}

#[test]
fn reverse_split_preserves_basis() {
    let l = ledger_of(vec![
        fill("2026-07-24T15:00:00Z", "GPUS", "buy", 1000.0, 0.124, None),
        json!({"activity_type": "SPLIT", "date": "2026-08-25", "created_at": "2026-08-25T05:59:00Z", "symbol": "GPUS", "qty": "-1000", "corporate_action_id": "x"}),
        json!({"activity_type": "SPLIT", "date": "2026-08-25", "created_at": "2026-08-25T06:07:00Z", "symbol": "GPUS", "qty": "200", "corporate_action_id": "x"}),
    ]);
    let lot = &l.open_lots["GPUS"][0];
    assert!(approx(lot.qty, 200.0) && approx(lot.price * lot.qty, 124.0));
}

#[test]
fn strategy_classification() {
    let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (vec![("X260116C00100000", "buy"), ("X260116C00105000", "sell")], "Bull call spread"),
        (vec![("X260116P00100000", "buy"), ("X260116P00095000", "sell")], "Bear put spread"),
        (vec![("X260116P00100000", "sell"), ("X260116P00095000", "buy")], "Bull put spread"),
        (vec![("X260116C00100000", "buy"), ("X260116P00100000", "buy")], "Long straddle"),
        (vec![("X260116C00110000", "sell"), ("X260116C00115000", "buy"), ("X260116P00090000", "sell"), ("X260116P00085000", "buy")], "Iron condor"),
        (vec![("X260116C00100000", "sell"), ("X260220C00100000", "buy")], "Calendar spread"),
    ];
    for (legs, want) in cases {
        let l = ledger_of(legs.iter().map(|(s, side)| fill("2026-01-05T15:00:00Z", s, side, 1.0, 1.0, None)).collect());
        assert_eq!(l.combos.iter().map(|c| c.strategy.as_str()).collect::<Vec<_>>(), vec![want]);
    }
}

#[test]
fn roll_detected_regardless_of_leg_order() {
    let l = ledger_of(vec![
        fill("2026-01-05T15:00:00Z", "X260116C00100000", "buy", 1.0, 2.0, None),
        fill("2026-01-09T15:00:00Z", "X260123C00100000", "buy", 1.0, 2.5, None),
        fill("2026-01-09T15:00:00Z", "X260116C00100000", "sell", 1.0, 1.0, None),
    ]);
    assert_eq!(l.combo(2).rolled_from, Some(1));
    assert_eq!(l.combo(1).rolled_to, Some(2));
}

#[test]
fn partial_fills_count_as_one_trade() {
    let l = ledger_of(vec![
        fill("2026-01-05T15:00:00Z", "XYZ", "buy", 89.0, 1.70, Some("o1")),
        fill("2026-01-05T15:00:01Z", "XYZ", "buy", 1.0, 1.70, Some("o1")),
        fill("2026-01-05T15:00:03Z", "XYZ", "buy", 10.0, 1.71, Some("o1")),
        fill("2026-01-06T15:00:00Z", "XYZ", "sell", 100.0, 2.0, Some("o2")),
    ]);
    let t = l.closed_df();
    assert_eq!(t.len(), 1);
    assert!(approx(t[0].qty, 100.0));
}

#[test]
fn time_weighted_return_ignores_deposits() {
    use alpaca_engine::metrics::{add_returns, EqFrame};
    let mut f = EqFrame {
        dates: (5..=8).map(|d| date_ymd(2026, 1, d)).collect(),
        equity: vec![1000.0, 1100.0, 2100.0, 2310.0],
        flow: vec![1000.0, 0.0, 1000.0, 0.0],
        ..Default::default()
    };
    add_returns(&mut f);
    assert!(approx(*f.twr.last().unwrap(), 1.1 * 1.0 * 1.1));
    assert!(approx(*f.cum_pnl.last().unwrap(), 310.0));
}

/// Every fixture (bundles from the Python test-suite) builds both reports without panicking.
#[test]
fn fixtures_build() {
    let now = parse_ts("2026-11-23T15:00:00Z").unwrap();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut n = 0;
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().map(|x| x != "json").unwrap_or(true) {
            continue;
        }
        let b = fs::read_to_string(&p).unwrap();
        let o = fs::read_to_string(p.with_extension("opts")).unwrap_or_else(|_| "{}".into());
        let out = alpaca_engine::build_reports(&b, &o, now, 0).unwrap_or_else(|e| panic!("{}: {}", p.display(), e));
        let v: Value = serde_json::from_str(&out).unwrap();
        for view in ["full", "share"] {
            let html = v[view].as_str().unwrap();
            assert!(html.contains("const FIGS = {"), "{}", p.display());
            assert!(!html.contains(">NaN<") && !html.contains("$nan"), "{}: NaN in {}", p.display(), view);
        }
        let share = v["share"].as_str().unwrap();
        assert!(!share.contains("id=\"orders\""), "{}: orders tab in shared view", p.display());
        n += 1;
    }
    assert!(n >= 10);
}
