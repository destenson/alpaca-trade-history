//! Chance that an unfilled option order's limit is reached, by horizon: Monte Carlo on the
//! underlying (driftless GBM at the legs' IV) on the trading-day clock, re-pricing the combo every
//! half hour of each session with the calibrated leg models.

use chrono::{Duration, NaiveDate};

use crate::orders::PendingOrder;
use crate::reprice::{self as rp, Market};
use crate::util::{self, from_ny, ny_date};

pub const SESSION: f64 = 6.5 / 24.0;
pub const STEPS: usize = 13;
pub const STEPS_LATER: usize = 3;
pub const PATHS: usize = 10_000;       // simulated paths (antithetic pairs)
pub const HORIZONS: [usize; 5] = [1, 2, 3, 5, 8];
pub const GTC_DAYS: i64 = 90;
const GRID: usize = 256;         // log-price grid the spread is priced on at each check

#[derive(Clone, Debug)]
pub struct FillOdds {
    pub sessions: usize,
    pub gtc: bool,
    pub last_day: NaiveDate,
    pub h: [Option<f64>; 5],
    pub expiry: Option<f64>,
    /// chance the order fills (before it expires; this session for DAY orders)
    pub p_fill: f64,
    /// chance it fills and then finishes in profit at the first expiration
    pub fill_and_profit: f64,
    /// chance of a profit at the first expiration given that it filled (None if no path filled)
    pub profit_if_filled: Option<f64>,
    /// expected P&L at the first expiration given that it filled at the limit, whole order in $
    pub ev_if_filled: Option<f64>,
}

fn ncdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-z * z).exp();
    0.5 * (1.0 + x.signum() * y * if x == 0.0 { 0.0 } else { 1.0 })
}

fn bs(s: f64, k: f64, t: f64, iv: f64, right: char) -> f64 {
    if t <= 0.0 {
        return if right == 'C' { (s - k).max(0.0) } else { (k - s).max(0.0) };
    }
    let v = iv * t.sqrt();
    let d1 = ((s / k).ln() + (rp::R + 0.5 * iv * iv) * t) / v;
    let d2 = d1 - v;
    let df = (-rp::R * t).exp();
    if right == 'C' {
        s * ncdf(d1) - k * df * ncdf(d2)
    } else {
        k * df * ncdf(-d2) - s * ncdf(-d1)
    }
}

pub fn sessions_until(start: &util::Ts, end_day: NaiveDate) -> usize {
    let mut d = ny_date(start);
    let mut n = 0;
    while d <= end_day {
        if rp::is_trading_day(d) {
            n += 1;
        }
        d += Duration::days(1);
    }
    n
}

/// xoshiro256** seeded through splitmix64; normals by the polar method.
struct Rng {
    s: [u64; 4],
    spare: Option<f64>,
}

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        };
        Rng { s: [next(), next(), next(), next()], spare: None }
    }
    fn u64(&mut self) -> u64 {
        let r = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }
    fn uniform(&mut self) -> f64 {
        (self.u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
    fn normal(&mut self) -> f64 {
        if let Some(s) = self.spare.take() {
            return s;
        }
        loop {
            let (u, v) = (self.uniform(), self.uniform());
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let m = (-2.0 * s.ln() / s).sqrt();
                self.spare = Some(v * m);
                return u * m;
            }
        }
    }
}

pub fn fill_odds(po: &PendingOrder, market: &Market, iv_shift: f64) -> Option<FillOdds> {
    let legs: Vec<_> = po.legs.iter().filter(|l| l.is_option() && l.model.is_some()).collect();
    if po.net.is_none() || po.spot.map(|s| s == 0.0).unwrap_or(true) || legs.is_empty() || legs.len() != po.legs.len() {
        return None;
    }
    let net = po.net.unwrap();
    let s0 = market.underlyings.get(&po.underlying).and_then(|u| u.spot).filter(|s| *s != 0.0).or(po.spot).unwrap();
    let t0 = market.valuation;
    let mut first = 1.0;
    if market.is_open {
        let close = from_ny(ny_date(&t0), 16, 0, 0);
        first = (util::secs(&t0, &close) / (6.5 * 3600.0)).max(0.02);
    }
    let gtc = po.tif.to_lowercase() == "gtc";
    let mut last_day = legs.iter().map(|l| l.model.as_ref().unwrap().expiry).min().unwrap();
    if gtc {
        if let Some(sub) = po.submitted {
            last_day = last_day.min(ny_date(&(sub + Duration::days(GTC_DAYS))));
        }
    }
    let n_sess = if gtc { sessions_until(&t0, last_day).max(1) } else { 1 };
    let ivs: Vec<f64> = legs.iter().map(|l| l.model.as_ref().unwrap().iv).collect();
    let sigma = (util::mean(&ivs) * (1.0 + iv_shift)).max(0.05);

    let key = if po.id.is_empty() { po.legs_label() } else { po.id.clone() };
    let seed = u64::from_str_radix(&util::sha1_hex(&key)[..8], 16).unwrap_or(0);
    let mut rng = Rng::new(seed);
    let half = PATHS / 2;
    let np = 2 * half;
    let mut log_s = vec![s0.ln(); np];
    let mut alive = vec![true; np];
    let mut hit_at = vec![usize::MAX; np];
    let models: Vec<(f64, f64, f64, char, f64)> = legs
        .iter()
        .map(|l| {
            let m = l.model.as_ref().unwrap();
            (l.ratio, m.strike, m.t, m.right, (m.iv * (1.0 + iv_shift)).max(1e-3))
        })
        .collect();
    let value = |s: f64, elapsed: f64| -> f64 {
        models.iter().map(|(ratio, k, t, right, iv)| ratio * bs(s, *k, (t - elapsed / rp::TRADING_DAYS).max(0.0), *iv, *right)).sum()
    };
    let mut z = vec![0.0; half];
    let mut step = |log_s: &mut Vec<f64>, dt: f64, rng: &mut Rng| {
        for zi in z.iter_mut() {
            *zi = rng.normal();
        }
        let var = sigma * sigma * dt / rp::TRADING_DAYS;
        let sd = var.sqrt();
        for i in 0..half {
            log_s[i] += -0.5 * var + sd * z[i];
            log_s[i + half] += -0.5 * var - sd * z[i];
        }
    };
    let mut elapsed = 0.0;
    let tol = 1e-9;
    let mut any_alive = true;
    for k in 1..=n_sess {
        let sess = SESSION * if k == 1 { first } else { 1.0 };
        let m = if k <= *HORIZONS.last().unwrap() { STEPS } else { STEPS_LATER };
        for c in 0..=m {
            if c > 0 {
                let dt = sess / m as f64;
                step(&mut log_s, dt, &mut rng);
                elapsed += dt;
            }
            if !any_alive {
                break;
            }
            // Price the spread on a grid of log-prices spanning the live paths, then classify each path
            // from the grid; only paths in a grid cell where the limit is crossed are priced exactly.
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            for i in 0..np {
                if alive[i] {
                    lo = lo.min(log_s[i]);
                    hi = hi.max(log_s[i]);
                }
            }
            let dx = (hi - lo) / (GRID - 1) as f64;
            let fills: Vec<bool> = if dx > 1e-12 {
                (0..GRID).map(|g| value((lo + g as f64 * dx).exp(), elapsed) <= net + tol).collect()
            } else {
                vec![]
            };
            any_alive = false;
            for i in 0..np {
                if !alive[i] {
                    continue;
                }
                let hit = if fills.is_empty() {
                    value(log_s[i].exp(), elapsed) <= net + tol
                } else {
                    let g = (((log_s[i] - lo) / dx) as usize).min(GRID - 2);
                    if fills[g] == fills[g + 1] { fills[g] } else { value(log_s[i].exp(), elapsed) <= net + tol }
                };
                if hit {
                    hit_at[i] = k;
                    alive[i] = false;
                } else {
                    any_alive = true;
                }
            }
        }
        if k < n_sess {
            let gap = 1.0 - SESSION;
            step(&mut log_s, gap, &mut rng);
            elapsed += gap;
        }
        if !any_alive {
            break;
        }
    }
    // carry every path on to the first expiration (one exact lognormal step) and settle the filled ones
    let t_end = models.iter().map(|m| m.2).fold(f64::INFINITY, f64::min) * rp::TRADING_DAYS;
    if t_end > elapsed {
        step(&mut log_s, t_end - elapsed, &mut rng);
    }
    let (mut n_fill, mut n_win, mut pnl) = (0usize, 0usize, 0.0);
    for i in 0..np {
        if hit_at[i] != usize::MAX {
            let p = value(log_s[i].exp(), t_end.max(elapsed)) - net;
            n_fill += 1;
            pnl += p;
            if p > tol {
                n_win += 1;
            }
        }
    }
    let frac = |h: usize| hit_at.iter().filter(|x| **x <= h).count() as f64 / np as f64;
    let mut hs = [None; 5];
    for (i, h) in HORIZONS.iter().enumerate() {
        if gtc || *h == 1 {
            hs[i] = Some(frac(*h));
        }
    }
    Some(FillOdds {
        sessions: n_sess,
        gtc,
        last_day,
        h: hs,
        expiry: if gtc { Some(n_fill as f64 / np as f64) } else { None },
        p_fill: n_fill as f64 / np as f64,
        fill_and_profit: n_win as f64 / np as f64,
        profit_if_filled: if n_fill > 0 { Some(n_win as f64 / n_fill as f64) } else { None },
        ev_if_filled: if n_fill > 0 { Some(pnl / n_fill as f64 * po.units * po.mult()) } else { None },
    })
}
