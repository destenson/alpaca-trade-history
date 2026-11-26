//! Black-Scholes pricing and payoff curves.

use chrono::NaiveDate;

use crate::occ::parse_occ;
use crate::util::{erf, round_dp};

fn ncdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

pub fn bs_price(s: f64, k: f64, t: f64, sigma: f64, right: char, r: f64) -> f64 {
    if t <= 0.0 || sigma <= 0.0 {
        return if right == 'C' { (s - k).max(0.0) } else { (k - s).max(0.0) };
    }
    let v = sigma * t.sqrt();
    let d1 = ((s / k).ln() + (r + 0.5 * sigma * sigma) * t) / v;
    let d2 = d1 - v;
    if right == 'C' {
        s * ncdf(d1) - k * (-r * t).exp() * ncdf(d2)
    } else {
        k * (-r * t).exp() * ncdf(-d2) - s * ncdf(-d1)
    }
}

#[derive(Clone, Debug)]
pub struct LegIn {
    pub symbol: String,
    pub qty: f64,
    pub cost: f64,
    pub iv: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct Curves {
    pub x: Vec<f64>,
    pub expiry: Vec<f64>,
    pub today: Vec<f64>,
    pub breakevens: Vec<f64>,
    /// price ranges where the P&L at expiration is positive; open ends are ±infinity
    pub zones: Vec<(f64, f64)>,
    pub max_profit: f64,
    pub max_loss: f64,
}

/// P&L vs underlying price: ``expiry`` = every leg at intrinsic; ``today`` = Black-Scholes at each
/// leg's IV (calendar days, as in the original).
pub fn payoff_curves(legs: &[LegIn], spot: f64, today: NaiveDate, shares: f64, share_cost: f64, xs: &[f64]) -> Curves {
    let _ = spot;
    let n = xs.len();
    let base = -legs.iter().map(|l| l.cost).sum::<f64>();
    let mut at_exp: Vec<f64> = xs.iter().map(|x| base + shares * x - share_cost).collect();
    let mut now = at_exp.clone();
    for l in legs {
        let c = parse_occ(&l.symbol).unwrap();
        let mult = 100.0 * l.qty;
        let t = ((c.expiry - today).num_days().max(0)) as f64 / 365.0 + 0.5 / 365.0;
        let iv = l.iv.filter(|v| *v != 0.0 && !v.is_nan()).unwrap_or(0.5);
        for i in 0..n {
            let x = xs[i];
            at_exp[i] += mult * if c.is_call() { (x - c.strike).max(0.0) } else { (c.strike - x).max(0.0) };
            now[i] += mult * bs_price(x, c.strike, t, iv, c.right, 0.04);
        }
    }
    let (be, zones, max_profit, max_loss) = exact_expiry(legs, shares, share_cost);
    Curves {
        x: xs.iter().map(|x| round_dp(*x, 4)).collect(),
        expiry: at_exp.iter().map(|x| round_dp(*x, 2)).collect(),
        today: now.iter().map(|x| round_dp(*x, 2)).collect(),
        breakevens: be,
        zones,
        max_profit,
        max_loss,
    }
}

/// The P&L at expiration, exactly. It is piecewise linear and bends only at strikes, so it is fully known
/// from its value at 0, at each strike and its slope above the highest strike: breakevens, profit zones
/// and max profit/loss are solved segment by segment (no sampling). Returns (breakevens, profit zones
/// with ±infinity for open ends, max profit, max loss); ±infinity when unbounded.
pub fn exact_expiry(legs: &[LegIn], shares: f64, share_cost: f64) -> (Vec<f64>, Vec<(f64, f64)>, f64, f64) {
    let cs: Vec<(f64, bool, f64)> = legs.iter().filter_map(|l| parse_occ(&l.symbol).map(|c| (c.strike, c.is_call(), 100.0 * l.qty))).collect();
    let base = -legs.iter().map(|l| l.cost).sum::<f64>() - share_cost;
    let pnl = |x: f64| base + shares * x + cs.iter().map(|&(k, call, m)| m * if call { (x - k).max(0.0) } else { (k - x).max(0.0) }).sum::<f64>();
    // slope above the highest strike: shares plus every call
    let slope_hi = shares + cs.iter().filter(|c| c.1).map(|c| c.2).sum::<f64>();
    let mut xs: Vec<f64> = vec![0.0];
    xs.extend(cs.iter().map(|c| c.0).filter(|k| *k > 0.0));
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let last = *xs.last().unwrap();
    let v_last = pnl(last);
    // one more point far enough above the last strike that any crossing in the tail lies before it
    let far = last + 1.0 + if slope_hi != 0.0 { 2.0 * (v_last / slope_hi).abs() } else { 0.0 };
    xs.push(far);
    let pts: Vec<(f64, f64)> = xs.iter().map(|&x| (x, pnl(x))).collect();
    // money sums carry float noise of ~1e-12: treat sub-micro-dollar values as exactly zero
    let sign = |v: f64| if v > 1e-6 { 1 } else if v < -1e-6 { -1 } else { 0 };
    // runs of constant sign covering [0, far]
    let mut runs: Vec<(f64, f64, i32)> = Vec::new();
    let push = |lo: f64, hi: f64, sg: i32, runs: &mut Vec<(f64, f64, i32)>| {
        if let Some(r) = runs.last_mut() {
            if r.2 == sg {
                r.1 = hi;
                return;
            }
        }
        runs.push((lo, hi, sg));
    };
    for w in pts.windows(2) {
        let ((xa, va), (xb, vb)) = (w[0], w[1]);
        let (sa, sb) = (sign(va), sign(vb));
        if sa == sb {
            push(xa, xb, sa, &mut runs);
        } else if sa == 0 {
            push(xa, xb, sb, &mut runs);
        } else if sb == 0 {
            push(xa, xb, sa, &mut runs);
        } else {
            let r = xa + (xb - xa) * (-va) / (vb - va);
            push(xa, r, sa, &mut runs);
            push(r, xb, sb, &mut runs);
        }
    }
    let zones: Vec<(f64, f64)> = runs
        .iter()
        .filter(|r| r.2 > 0)
        .map(|r| (if r.0 <= 0.0 { f64::NEG_INFINITY } else { round_dp(r.0, 2) }, if r.1 >= far { f64::INFINITY } else { round_dp(r.1, 2) }))
        .collect();
    // breakevens: where a winning run follows a losing one or the reverse (through any $0 stretch,
    // at the start of the run that follows it)
    let mut be = Vec::new();
    let mut prev: Option<i32> = None;
    for r in &runs {
        if r.2 == 0 {
            continue;
        }
        if let Some(p) = prev {
            if p != r.2 {
                be.push(round_dp(r.0, 2));
            }
        }
        prev = Some(r.2);
    }
    let vmax = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let vmin = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let max_profit = if slope_hi > 1e-9 { f64::INFINITY } else { vmax };
    let max_loss = if slope_hi < -1e-9 { f64::NEG_INFINITY } else { vmin };
    (be, zones, max_profit, max_loss)
}
