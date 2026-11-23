//! Analysis engine for the Alpaca trade-history web app.
//!
//! JavaScript fetches the account data, hands it over as one JSON document shaped like an on-disk
//! bundle, and gets back both the full and the shareable report HTML (see ``build_reports``).

pub mod analysis;
pub mod fillodds;
pub mod glossary;
pub mod interpret;
pub mod jv;
pub mod ledger;
pub mod metrics;
pub mod occ;
pub mod optmath;
pub mod orders;
pub mod reinvest;
pub mod reprice;
pub mod report;
pub mod share;
pub mod util;

use chrono::{Duration, NaiveDate};
use serde_json::{json, Value};

use analysis::Bundle;

pub fn bundle_from_json(d: &Value) -> Bundle {
    let arr = |k: &str| jv::arr(d.get(k)).to_vec();
    let obj_or_empty = |k: &str| match d.get(k) {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        _ => Value::Object(Default::default()),
    };
    let opt = |k: &str| match d.get(k) {
        Some(Value::Null) | None => None,
        Some(v) => Some(v.clone()),
    };
    let mut prices = indexmap::IndexMap::new();
    if let Some(Value::Object(o)) = d.get("prices") {
        for (k, v) in o {
            if let Some(x) = jv::float_of(v) {
                prices.insert(k.clone(), x);
            }
        }
    }
    Bundle {
        activities: arr("activities"),
        portfolio_history: opt("portfolio_history"),
        positions: arr("positions"),
        account: opt("account"),
        bars: obj_or_empty("bars"),
        prices,
        option_snapshots: obj_or_empty("option_snapshots"),
        orders: arr("orders"),
        stock_snapshots: obj_or_empty("stock_snapshots"),
        clock: opt("clock"),
    }
}

/// ``now``: the current instant; ``tz_offset_min``: the viewer's offset from UTC in minutes (east
/// positive) for the "generated" stamp.
pub fn build_reports(bundle_json: &str, options_json: &str, now: util::Ts, tz_offset_min: i64) -> Result<String, String> {
    let d: Value = serde_json::from_str(bundle_json).map_err(|e| format!("bad bundle JSON: {}", e))?;
    let opts: Value = serde_json::from_str(if options_json.trim().is_empty() { "{}" } else { options_json }).map_err(|e| format!("bad options JSON: {}", e))?;
    let b = bundle_from_json(&d);
    if b.activities.is_empty() {
        return Err("This account has no activity yet - nothing to analyze.".into());
    }
    let mut benchmarks: Vec<String> = jv::arr(opts.get("benchmarks")).iter().filter_map(|v| v.as_str().map(String::from)).collect();
    if benchmarks.is_empty() {
        benchmarks = vec!["SPY".into(), "QQQ".into()];
    }
    let window = jv::f(&opts, "combo_window").unwrap_or(120.0);
    let start = jv::opt_s(&opts, "start").and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
    let notional = jv::f(&opts, "notional").unwrap_or(10_000.0);
    let a = analysis::run(b, &benchmarks, window, start, 0.0);
    let local = now.naive_utc() + Duration::minutes(tz_offset_min);
    let generated = local.format("%Y-%m-%d %H:%M").to_string();
    let full = report::Report::new(&a, now, generated.clone()).to_html();
    let sh = share::make_shareable(&a, notional);
    let shared = report::Report::new(&sh, now, generated).to_html();
    let summary = a.stats.as_ref().map(|s| {
        json!({"equity": report::jf(s.ending_equity), "pnl": report::jf(s.pnl), "twr": report::jf(s.total_return), "max_dd": report::jf(s.max_dd),
               "start": util::iso_date(s.start), "end": util::iso_date(s.end)})
    });
    Ok(json!({"full": full, "share": shared, "summary": summary, "warnings": a.warnings, "alerts": a.alerts}).to_string())
}

// ------------------------------------------------------------------------------- wasm interface
// A tiny C-style ABI so the page needs no generated glue: JS copies two UTF-8 strings into memory
// allocated with ``aa_alloc``, calls ``aa_run`` and reads the JSON result back with ``aa_out_ptr``
// / ``aa_out_len``. A panic aborts the call; its message is kept for ``aa_panic_ptr``/``aa_panic_len``.

use std::sync::Mutex;

static OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static PANIC: Mutex<Vec<u8>> = Mutex::new(Vec::new());

#[no_mangle]
pub extern "C" fn aa_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// ``p`` must come from ``aa_alloc(n)`` and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn aa_free(p: *mut u8, n: usize) {
    drop(Vec::from_raw_parts(p, 0, n.max(1)));
}

/// # Safety
/// The pointers must reference ``*_len`` initialized bytes allocated with ``aa_alloc``.
#[no_mangle]
pub unsafe extern "C" fn aa_run(bp: *const u8, bl: usize, op: *const u8, ol: usize, now_ms: f64, tz_offset_min: f64) -> i32 {
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut p) = PANIC.lock() {
            *p = info.to_string().into_bytes();
        }
    }));
    let b = std::str::from_utf8(std::slice::from_raw_parts(bp, bl)).unwrap_or("");
    let o = std::str::from_utf8(std::slice::from_raw_parts(op, ol)).unwrap_or("{}");
    let now = chrono::DateTime::from_timestamp_millis(now_ms as i64).unwrap_or_default();
    let (code, out) = match build_reports(b, o, now, tz_offset_min as i64) {
        Ok(s) => (0, s),
        Err(e) => (1, json!({"error": e}).to_string()),
    };
    *OUT.lock().unwrap() = out.into_bytes();
    code
}

#[no_mangle]
pub extern "C" fn aa_out_ptr() -> *const u8 {
    OUT.lock().unwrap().as_ptr()
}

#[no_mangle]
pub extern "C" fn aa_out_len() -> usize {
    OUT.lock().unwrap().len()
}

#[no_mangle]
pub extern "C" fn aa_panic_ptr() -> *const u8 {
    PANIC.lock().map(|p| p.as_ptr()).unwrap_or(std::ptr::null())
}

#[no_mangle]
pub extern "C" fn aa_panic_len() -> usize {
    PANIC.lock().map(|p| p.len()).unwrap_or(0)
}
