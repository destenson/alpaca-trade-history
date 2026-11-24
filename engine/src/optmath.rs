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
    // Breakevens: where the P&L changes sign. Values within a cent of zero count as zero, so a P&L that
    // sits at exactly $0 over a range (e.g. an inverted iron condor whose credit equals its losses) adds
    // no rounding-noise crossings; touching zero and turning back isn't a breakeven either.
    let sign = |v: f64| if v > 0.005 { 1 } else if v < -0.005 { -1 } else { 0 };
    let mut be = Vec::new();
    let mut last: Option<(usize, i32)> = None;     // last point with a clear sign
    for i in 0..n {
        let s = sign(at_exp[i]);
        if s == 0 {
            continue;
        }
        if let Some((j, ls)) = last {
            if ls != s {
                be.push(round_dp(if j + 1 == i {
                    xs[j] - at_exp[j] * (xs[i] - xs[j]) / (at_exp[i] - at_exp[j])
                } else {
                    xs[i - 1]                       // left a $0 stretch on the losing/winning side
                }, 2));
            }
        }
        last = Some((i, s));
    }
    let max_profit = at_exp.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let max_loss = at_exp.iter().cloned().fold(f64::INFINITY, f64::min);
    Curves {
        x: xs.iter().map(|x| round_dp(*x, 4)).collect(),
        expiry: at_exp.iter().map(|x| round_dp(*x, 2)).collect(),
        today: now.iter().map(|x| round_dp(*x, 2)).collect(),
        breakevens: be,
        max_profit,
        max_loss,
    }
}
