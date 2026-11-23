//! Lenient accessors for the raw Alpaca JSON.

use serde_json::Value;

pub fn get<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    match v.get(k) {
        Some(Value::Null) | None => None,
        Some(x) => Some(x),
    }
}

/// Python ``float(x)`` that yields None on failure.
pub fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok()
            }
        }
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn f(v: &Value, k: &str) -> Option<f64> {
    get(v, k).and_then(float_of)
}

pub fn f0(v: &Value, k: &str) -> f64 {
    f(v, k).unwrap_or(0.0)
}

/// Finite, else None (reprice/orders ``_f`` variants also drop non-positive values where noted).
pub fn ff(v: &Value, k: &str) -> Option<f64> {
    f(v, k).filter(|x| x.is_finite())
}

pub fn fpos(v: &Value, k: &str) -> Option<f64> {
    ff(v, k).filter(|x| *x > 0.0)
}

/// Value as a string ('' when missing or null; numbers rendered).
pub fn s(v: &Value, k: &str) -> String {
    match get(v, k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).to_string(),
        _ => String::new(),
    }
}

pub fn opt_s(v: &Value, k: &str) -> Option<String> {
    let x = s(v, k);
    if x.is_empty() {
        None
    } else {
        Some(x)
    }
}

pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub fn arr(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    }
}

pub fn obj(v: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    match v {
        Some(Value::Object(o)) => Some(o),
        _ => None,
    }
}

pub fn empty_obj() -> &'static Value {
    use std::sync::OnceLock;
    static E: OnceLock<Value> = OnceLock::new();
    E.get_or_init(|| Value::Object(serde_json::Map::new()))
}
