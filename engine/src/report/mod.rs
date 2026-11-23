//! Render an Analysis to one self-contained HTML file. Charts are Plotly JSON specs; colors are
//! ``var:<token>`` strings resolved in the browser from CSS custom properties.

mod options;
mod orders;
mod overview;
mod trades;

use chrono::NaiveDate;
use serde_json::{json, Map, Value};

use crate::analysis::Analysis;
use crate::glossary::tip;
use crate::occ::parse_occ;
use crate::reprice::{self as rp, Market};
use crate::util::{self, esc, money, py_float, Ts};

pub const PLOTLY_CDN: &str = "https://cdn.jsdelivr.net/npm/plotly.js-dist-min@2.35.2/plotly.min.js";
pub const PLOTLY_SRI: &str = "sha384-cCVCZkAjYNxaYKbM8lsArLznDF/SvMFr1jcZrvOpSTCa0W40ZAdLzHCEulnUa5i7";
const TEMPLATE: &str = include_str!("../../templates/report.html");

pub const S: [&str; 8] = ["var:series-1", "var:series-2", "var:series-3", "var:series-4", "var:series-5", "var:series-6", "var:series-7", "var:series-8"];
pub const GAIN: &str = "var:gain";
pub const LOSS: &str = "var:loss";
pub const NEUTRAL: &str = "var:muted";

pub fn pn(v: f64) -> &'static str {
    if v >= 0.0 {
        GAIN
    } else {
        LOSS
    }
}

/// A float for JSON (NaN / inf -> null).
pub fn jf(x: f64) -> Value {
    if x.is_finite() {
        json!(x)
    } else {
        Value::Null
    }
}

pub fn jr(x: f64, d: i32) -> Value {
    jf(util::round_dp(x, d))
}

pub fn jvec(v: &[f64], d: i32) -> Value {
    Value::Array(v.iter().map(|x| jr(*x, d)).collect())
}

pub fn jraw(v: &[f64]) -> Value {
    Value::Array(v.iter().map(|x| jf(*x)).collect())
}

pub fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

// ------------------------------------------------------------------------------------- tables
#[derive(Clone, Debug)]
pub enum Cell {
    S(String),
    F(f64),
    I(i64),
    N,
}

impl Cell {
    pub fn f(&self) -> f64 {
        match self {
            Cell::F(x) => *x,
            Cell::I(i) => *i as f64,
            _ => f64::NAN,
        }
    }
    pub fn s(x: impl Into<String>) -> Cell {
        Cell::S(x.into())
    }
    pub fn of(x: Option<f64>) -> Cell {
        match x {
            Some(v) => Cell::F(v),
            None => Cell::N,
        }
    }
}

pub type FmtFn<'a> = Box<dyn Fn(&Cell) -> String + 'a>;

pub struct Tbl<'a> {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub fmt: Vec<(String, FmtFn<'a>)>,
}

impl<'a> Tbl<'a> {
    pub fn new(cols: &[&str]) -> Tbl<'a> {
        Tbl { cols: cols.iter().map(|c| c.to_string()).collect(), rows: vec![], fmt: vec![] }
    }
    pub fn with_cols(cols: Vec<String>) -> Tbl<'a> {
        Tbl { cols, rows: vec![], fmt: vec![] }
    }
    pub fn f(mut self, col: &str, f: impl Fn(&Cell) -> String + 'a) -> Self {
        self.fmt.push((col.to_string(), Box::new(f)));
        self
    }
    pub fn push(&mut self, row: Vec<Cell>) {
        self.rows.push(row);
    }
    pub fn drop_cols(&mut self, drop: &[&str]) {
        let keep: Vec<usize> = (0..self.cols.len()).filter(|&i| !drop.contains(&self.cols[i].as_str())).collect();
        self.cols = keep.iter().map(|&i| self.cols[i].clone()).collect();
        for r in self.rows.iter_mut() {
            *r = keep.iter().map(|&i| r[i].clone()).collect();
        }
    }
}

/// Formatter shorthands for table columns.
pub fn fm(signed: bool, dp: usize) -> impl Fn(&Cell) -> String {
    move |c: &Cell| money(c.f(), signed, dp)
}
pub fn fp(signed: bool, dp: usize) -> impl Fn(&Cell) -> String {
    move |c: &Cell| util::pct(c.f(), signed, dp)
}
pub fn fnum(dp: usize) -> impl Fn(&Cell) -> String {
    move |c: &Cell| util::num(c.f(), dp)
}
pub fn fg() -> impl Fn(&Cell) -> String {
    |c: &Cell| util::g(c.f())
}
pub fn flim() -> impl Fn(&Cell) -> String {
    |c: &Cell| lim(c.f())
}

const CASH_OUT_COLS: [&str; 2] = ["Cost", "Cost basis"];

/// '+$252.41' / '−12.5%' -> ('$252.41' / '12.5%', sign): the cell's color shows the sign.
pub fn unsign(txt: &str) -> (String, Option<char>) {
    let mut it = txt.chars();
    let Some(first) = it.next() else { return (txt.to_string(), None) };
    if !matches!(first, '+' | '−' | '-') {
        return (txt.to_string(), None);
    }
    let rest: String = it.collect();
    let num_ok = |s: &str| -> bool {
        // \d[\d,]*(?:\.\d+)?
        let b: Vec<char> = s.chars().collect();
        if b.is_empty() || !b[0].is_ascii_digit() {
            return false;
        }
        let mut i = 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == ',') {
            i += 1;
        }
        if i == b.len() {
            return true;
        }
        if b[i] != '.' || i + 1 >= b.len() {
            return false;
        }
        b[i + 1..].iter().all(|c| c.is_ascii_digit())
    };
    let ok = if let Some(r) = rest.strip_prefix('$') {
        let r2 = r.strip_prefix(|c: char| c.is_whitespace()).unwrap_or(r);
        num_ok(r2)
    } else if let Some(r) = rest.strip_suffix('%') {
        num_ok(r)
    } else {
        false
    };
    if !ok {
        return (txt.to_string(), None);
    }
    (rest.replace("$ ", "$"), Some(if first == '+' { '+' } else { '−' }))
}

// ------------------------------------------------------------------------------ ticker links
thread_local! {
    /// underlying -> TradingView exchange prefix, from the exchanges Alpaca reports for open positions
    static EXCHANGES: std::cell::RefCell<std::collections::HashMap<String, String>> = Default::default();
}

pub fn set_exchanges(positions: &[Value]) {
    EXCHANGES.with(|m| {
        let mut m = m.borrow_mut();
        m.clear();
        for p in positions {
            let sym = crate::jv::s(p, "symbol");
            let tv = match crate::jv::s(p, "exchange").to_uppercase().as_str() {
                "NASDAQ" => "NASDAQ",
                "NYSE" => "NYSE",
                "ARCA" | "NYSEARCA" | "AMEX" => "AMEX",
                "BATS" => "CBOE",
                "OTC" => "OTC",
                _ => continue,
            };
            if parse_occ(&sym).is_none() {
                m.insert(sym, tv.to_string());
            }
        }
    });
}

fn tv_symbol(und: &str) -> String {
    EXCHANGES.with(|m| m.borrow().get(und).map(|e| format!("{}:{}", e, und))).unwrap_or_else(|| und.to_string())
}

fn is_ticker(s: &str) -> bool {
    !s.is_empty() && s.len() <= 10 && s.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'.' || c == b'/')
        && s.bytes().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
}

/// ``text`` (a ticker or an option symbol) linked to Alpaca's trade page for its underlying, followed by a
/// chart icon linking to TradingView (hovering it shows a mini chart). Plain escaped text if it isn't a ticker.
pub fn ticker(text: &str) -> String {
    let und = crate::occ::underlying_of(text);
    if !is_ticker(&und) {
        return esc(text);
    }
    let tv = tv_symbol(&und);
    format!(
        "<a class=\"tk\" href=\"https://app.alpaca.markets/trade/{u}?asset_class=stocks\" target=\"_blank\" rel=\"noopener noreferrer\" title=\"Trade {u} on Alpaca\">{t}</a><a class=\"tv\" href=\"https://www.tradingview.com/chart/?symbol={q}\" target=\"_blank\" rel=\"noopener noreferrer\" data-tv=\"{tv}\" aria-label=\"{u} chart on TradingView\">📈</a>",
        u = esc(&und),
        t = esc(text),
        q = tv.replace(':', "%3A"),
        tv = esc(&tv)
    )
}

const TICKER_COLS: [&str; 3] = ["Underlying", "Symbol", "symbol"];

/// Table cells: the $ is set in a lighter color with a small gap so it isn't misread as a 5.
pub fn cur(escaped: &str) -> String {
    escaped.replace('$', "<span class=\"cur\">$</span>")
}

pub fn tip_attr(label: &str) -> String {
    match tip(label) {
        Some(t) => format!(" data-tip=\"{}\" tabindex=\"0\"", esc(t)),
        None => String::new(),
    }
}

pub fn table(t: &Tbl, tid: &str, pnl_cols: &[&str]) -> String {
    if t.rows.is_empty() {
        return "<p class=\"empty\">No rows.</p>".into();
    }
    let th: String = t.cols.iter().enumerate().map(|(i, c)| format!("<th data-k=\"{}\"{}>{}</th>", i, tip_attr(c), esc(c))).collect();
    let mut body = String::new();
    for r in &t.rows {
        body.push_str("<tr>");
        for (c, v) in t.cols.iter().zip(r) {
            let f = t.fmt.iter().find(|(k, _)| k == c).map(|(_, f)| f);
            let txt = match f {
                Some(f) => f(v),
                None => match v {
                    Cell::N => "–".into(),
                    Cell::F(x) if x.is_nan() => "–".into(),
                    Cell::F(x) => py_float(*x),
                    Cell::I(i) => i.to_string(),
                    Cell::S(s) => s.clone(),
                },
            };
            let (txt, sign) = unsign(&txt);
            let raw = match v {
                Cell::F(x) if !x.is_nan() => py_float(*x),
                Cell::I(i) => i.to_string(),
                _ => txt.clone(),
            };
            let mut cls = "";
            if pnl_cols.contains(&c.as_str()) {
                let x = v.f();
                if !x.is_nan() {
                    cls = if x > 0.0 {
                        " class=\"pos\""
                    } else if x < 0.0 {
                        " class=\"neg\""
                    } else {
                        ""
                    };
                }
            }
            if let Some(sg) = sign {
                let neg = (sg == '−') != CASH_OUT_COLS.contains(&c.as_str());
                cls = if neg { " class=\"neg\"" } else { " class=\"pos\"" };
            }
            let html = if TICKER_COLS.contains(&c.as_str()) { ticker(&txt) } else { cur(&esc(&txt)) };
            body.push_str(&format!("<td data-v=\"{}\"{}>{}</td>", esc(&raw), cls, html));
        }
        body.push_str("</tr>");
    }
    format!("<div class=\"tablewrap\"><table class=\"sortable\" id=\"{}\"><thead><tr>{}</tr></thead><tbody>{}</tbody></table></div>", tid, th, body)
}

// ------------------------------------------------------------------------------------ helpers
pub fn lim(v: f64) -> String {
    if v.is_nan() {
        "–".into()
    } else if v.is_infinite() {
        "unlimited".into()
    } else {
        money(v, true, 2)
    }
}

pub fn iso(t: &Ts) -> String {
    util::iso_ts(t)
}

pub fn isod(d: NaiveDate) -> String {
    util::iso_date(d)
}

pub fn leg(sym: &str) -> String {
    match parse_occ(sym) {
        Some(c) => format!("{} {} {}{}", c.underlying, c.expiry.format("%m/%d"), util::g(c.strike), c.right),
        None => sym.to_string(),
    }
}

pub fn slug(parts: &[&str]) -> String {
    let raw = parts.join("|");
    let head: String = parts.first().map(|p| p.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).take(8).collect()).unwrap_or_default();
    format!("{}_{}", head, &util::sha1_hex(&raw)[..8])
}

pub fn when(m: &Market) -> String {
    if m.is_open {
        return "now".into();
    }
    let v = util::to_ny(&m.valuation);
    format!("the next open ({} {}, {} ET)", v.format("%a %b"), chrono::Datelike::day(&v), util::clock(&v))
}

pub fn dollars_list(v: &[f64]) -> String {
    v.iter().map(|b| format!("${}", util::commas(*b, 2))).collect::<Vec<_>>().join(", ")
}

pub fn spot_from(a: &Analysis, und: &str) -> Option<f64> {
    if let Some(p) = a.open_positions.iter().find(|p| p.symbol == und && !p.price.is_nan()) {
        return Some(p.price);
    }
    if let Some(p) = a.bundle.prices.get(und) {
        return Some(*p);
    }
    let bars = crate::jv::arr(a.bundle.bars.get(und));
    bars.last().and_then(|b| crate::jv::f(b, "c"))
}

pub fn dur(days: f64) -> String {
    if days.is_nan() {
        return "–".into();
    }
    if days < 1.0 / 24.0 {
        return format!("{:.0} min", days * 1440.0);
    }
    if days < 1.0 {
        return format!("{:.1} h", days * 24.0);
    }
    format!("{:.1} d", days)
}

pub fn profit_factor(pnl: &[f64]) -> f64 {
    if pnl.is_empty() {
        return f64::NAN;
    }
    let loss: f64 = -pnl.iter().filter(|p| **p < 0.0).sum::<f64>();
    let gain: f64 = pnl.iter().filter(|p| **p > 0.0).sum();
    if loss > 0.0 {
        gain / loss
    } else {
        f64::INFINITY
    }
}

// ------------------------------------------------------------------------------------- report
pub struct Report<'a> {
    pub a: &'a Analysis,
    pub figs: Map<String, Value>,
    pub sections: Vec<(String, String, String)>,
    pub share: bool,
    pub notional: f64,
    pub market: Option<Market>,
    pub market_ready: bool,
    pub rp_views: Vec<Value>,
    pub now: Ts,
    pub generated: String,
}

impl<'a> Report<'a> {
    pub fn new(a: &'a Analysis, now: Ts, generated: String) -> Report<'a> {
        set_exchanges(&a.bundle.positions);
        Report {
            a,
            figs: Map::new(),
            sections: vec![],
            share: a.share,
            notional: a.notional,
            market: None,
            market_ready: false,
            rp_views: vec![],
            now,
            generated,
        }
    }

    pub fn today(&self) -> NaiveDate {
        util::ny_date(&self.now)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fig(&mut self, fid: &str, data: Vec<Value>, layout: Value, height: i64, note: &str, title: &str, subtitle: &str, wide: bool) -> String {
        let shown = data.iter().filter(|d| d.get("showlegend") != Some(&Value::Bool(false))).count();
        let mut lay = obj(json!({"height": height, "margin": {"l": 56, "r": 16, "t": 12, "b": 40}, "hovermode": "closest", "showlegend": shown > 1}));
        for (k, v) in obj(layout) {
            lay.insert(k, v);
        }
        self.figs.insert(fid.to_string(), json!({"data": data, "layout": Value::Object(lay)}));
        let head = if title.is_empty() { String::new() } else { format!("<h3>{}</h3>", esc(title)) };
        let sub = if subtitle.is_empty() { String::new() } else { format!("<p class=\"sub\">{}</p>", subtitle) };
        let foot = if note.is_empty() { String::new() } else { format!("<p class=\"note\">{}</p>", note) };
        format!("<figure class=\"card{}\">{}{}<div class=\"plot\" id=\"{}\"></div>{}</figure>", if wide { " wide" } else { "" }, head, sub, fid, foot)
    }

    pub fn tiles(items: &[(String, String, String)]) -> String {
        let out: String = items
            .iter()
            .map(|(l, v, d)| format!("<div class=\"tile\"><div class=\"tl\"{}>{}</div><div class=\"tv\">{}</div><div class=\"td\">{}</div></div>", tip_attr(l), esc(l), v, d))
            .collect();
        format!("<div class=\"tiles\">{}</div>", out)
    }

    pub fn section(&mut self, sid: &str, title: &str, body: String) {
        self.sections.push((sid.into(), title.into(), body));
    }

    /// Market state for re-pricing (shared by the Options and Orders tabs); None in the shared view.
    pub fn market(&mut self) -> Option<&Market> {
        if self.share {
            return None;
        }
        if !self.market_ready {
            let a = self.a;
            let today = self.today();
            let mut unds: Vec<String> = crate::orders::parse_orders(&a.bundle.orders).into_iter().filter(|o| o.is_option).map(|o| o.underlying).collect();
            for p in &a.open_positions {
                if p.is_option && p.expiry.map(|e| e >= today).unwrap_or(false) {
                    unds.push(p.underlying.clone());
                }
            }
            unds.sort();
            unds.dedup();
            let qt = rp::quote_times(&a.bundle.option_snapshots);
            let fb: std::collections::HashMap<String, Option<f64>> = unds.iter().map(|u| (u.clone(), spot_from(a, u))).collect();
            self.market = Some(rp::market_state(a.bundle.clock.as_ref(), Some(&a.bundle.stock_snapshots), &unds, &qt, &fb, &self.now));
            self.market_ready = true;
            self.rp_views = vec![];
        }
        self.market.as_ref()
    }

    pub fn build(&mut self) {
        self.overview();
        self.performance();
        self.drawdown();
        self.trades();
        self.options();
        if !self.share {
            self.orders();
        }
        self.positions();
        self.notes();
    }

    pub fn to_html(mut self) -> String {
        self.build();
        if !self.rp_views.is_empty() {
            let m = self.market.as_ref().unwrap();
            let payload = json!({"r": rp::R, "when": when(m), "is_open": m.is_open, "views": self.rp_views,
                                 "est_col": if m.is_open { "Est. now" } else { "Est. at open" }});
            let js = serde_json::to_string(&payload).unwrap().replace("</", "<\\/");
            if let Some(last) = self.sections.last_mut() {
                last.2.push_str(&format!("<script type=\"application/json\" id=\"rp-data\">{}</script>", js));
            }
        }
        let nav: String = self.sections.iter().map(|(sid, t, _)| format!("<a href=\"#{}\" data-tab=\"{}\">{}</a>", sid, sid, esc(t))).collect();
        let body: String = self.sections.iter().map(|(sid, t, b)| format!("<section id=\"{}\" class=\"tab\"><h2>{}</h2>{}</section>", sid, esc(t), b)).collect();
        let mut span = String::new();
        if let Some(e) = &self.a.equity {
            span = format!("{} – {}", e.dates[0].format("%b %d, %Y"), e.dates.last().unwrap().format("%b %d, %Y"));
        }
        if self.share {
            span.push_str(&format!(" · shared view: $ normalized to a {} account", money(self.notional, false, 0)));
        }
        let script = format!("<script src=\"{}\" integrity=\"{}\" crossorigin=\"anonymous\"></script>", PLOTLY_CDN, PLOTLY_SRI);
        let figs = serde_json::to_string(&self.figs).unwrap().replace("</", "<\\/");
        let generated = if self.share { self.generated.chars().take(10).collect() } else { self.generated.clone() };
        TEMPLATE
            .replace("{{NAV}}", &nav)
            .replace("{{BODY}}", &body)
            .replace("{{SPAN}}", &esc(&span))
            .replace("{{GENERATED}}", &generated)
            .replace("{{PLOTLY}}", &script)
            .replace("{{FIGS}}", &figs)
    }
}
