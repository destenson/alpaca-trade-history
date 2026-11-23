//! Native runner used for parity checks: ``aa-report <bundle-dir | bundle.json> [options.json]``.
//! Prints the same JSON the web app gets ({full, share, summary, warnings, alerts}).

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

fn read_csv(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect();
    if lines.is_empty() {
        return vec![];
    }
    let split = |l: &str| -> Vec<String> {
        let mut out = vec![];
        let mut cur = String::new();
        let mut q = false;
        let mut chars = l.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if q && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => q = !q,
                ',' if !q => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
        out.push(cur);
        out
    };
    let head: Vec<String> = split(lines[0]).into_iter().map(|h| h.trim().to_string()).collect();
    let mut out = vec![];
    for l in &lines[1..] {
        let cells = split(l);
        let row: std::collections::HashMap<&str, String> = head.iter().map(|h| h.as_str()).zip(cells.iter().map(|c| c.trim().to_string())).collect();
        let g = |k: &str| row.get(k).cloned().unwrap_or_default();
        let t = g("activity_type").to_uppercase();
        let mut rec = Map::new();
        rec.insert("activity_type".into(), Value::String(t.clone()));
        if t == "FILL" {
            for (k, src) in [("transaction_time", "time"), ("symbol", "symbol"), ("side", "side"), ("qty", "qty"), ("price", "price")] {
                rec.insert(k.into(), Value::String(g(src)));
            }
            let oid = g("order_id");
            rec.insert("order_id".into(), if oid.is_empty() { Value::Null } else { Value::String(oid) });
            rec.insert("type".into(), Value::String("fill".into()));
        } else {
            let time = g("time");
            rec.insert("date".into(), Value::String(time.chars().take(10).collect()));
            if time.len() > 10 {
                rec.insert("created_at".into(), Value::String(time));
            }
            for k in ["symbol", "qty", "price", "net_amount"] {
                if !g(k).is_empty() {
                    rec.insert(k.into(), Value::String(g(k)));
                }
            }
            if !g("sub_type").is_empty() {
                rec.insert("activity_sub_type".into(), Value::String(g("sub_type")));
            }
            if !g("order_id").is_empty() {
                rec.insert("corporate_action_id".into(), Value::String(g("order_id")));
            }
        }
        out.push(Value::Object(rec));
    }
    out
}

fn load_dir(p: &Path) -> Value {
    let j = |name: &str| -> Option<Value> { fs::read_to_string(p.join(name)).ok().and_then(|s| serde_json::from_str(&s).ok()) };
    let mut acts: Vec<Value> = j("activities.json").and_then(|v| v.as_array().cloned()).unwrap_or_default();
    if p.join("activities.csv").exists() {
        acts.extend(read_csv(&p.join("activities.csv")));
    }
    let mut m = Map::new();
    m.insert("activities".into(), Value::Array(acts));
    for (k, f) in [
        ("portfolio_history", "portfolio_history.json"),
        ("positions", "positions.json"),
        ("account", "account.json"),
        ("bars", "bars.json"),
        ("prices", "prices.json"),
        ("option_snapshots", "option_snapshots.json"),
        ("orders", "orders.json"),
        ("stock_snapshots", "stock_snapshots.json"),
        ("clock", "clock.json"),
    ] {
        if let Some(v) = j(f) {
            m.insert(k.into(), v);
        }
    }
    Value::Object(m)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: aa-report <bundle-dir | bundle.json> [options.json]");
        std::process::exit(2);
    }
    let p = Path::new(&args[1]);
    let bundle = if p.is_dir() { load_dir(p).to_string() } else { fs::read_to_string(p).expect("read bundle") };
    let opts = args.get(2).map(|f| fs::read_to_string(f).expect("read options")).unwrap_or_else(|| "{}".into());
    let now = match std::env::var("AA_NOW") {
        Ok(s) => alpaca_engine::util::parse_ts(&s).expect("AA_NOW"),
        Err(_) => chrono::DateTime::from_timestamp_millis(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64).unwrap(),
    };
    let t0 = std::time::Instant::now();
    match alpaca_engine::build_reports(&bundle, &opts, now, 0) {
        Ok(s) => {
            eprintln!("built in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
            println!("{}", s)
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}
