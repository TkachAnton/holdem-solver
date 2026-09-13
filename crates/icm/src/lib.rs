//! Точный ICM (Independent Chip Model) калькулятор по Malmuth-Harville.
//!
//! Модель: вероятность каждого порядка финишей пропорциональна произведению
//! долей стеков выбывающих. Вероятности мест вычисляются динамическим
//! программированием по подмножествам игроков: O(2^n * n).

use std::fmt;

pub type Chips = i64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcmError {
    EmptyStacks,
    EmptyPayouts,
    MorePayoutsThanPlayers,
    NonPositiveStack(usize),
    NegativePayout(usize),
}

impl fmt::Display for IcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IcmError::EmptyStacks => write!(f, "stacks cannot be empty"),
            IcmError::EmptyPayouts => write!(f, "payouts cannot be empty"),
            IcmError::MorePayoutsThanPlayers => {
                write!(f, "number of payouts cannot exceed number of players")
            }
            IcmError::NonPositiveStack(idx) => {
                write!(f, "stack at index {} must be positive", idx)
            }
            IcmError::NegativePayout(idx) => {
                write!(f, "payout at index {} cannot be negative", idx)
            }
        }
    }
}

impl std::error::Error for IcmError {}

#[derive(Debug, Clone, PartialEq)]
pub struct IcmResult {
    pub ev: Vec<f64>,
    pub place_probs: Vec<Vec<f64>>,
}

pub fn icm_equity(stacks: &[Chips], payouts: &[Chips]) -> Result<IcmResult, IcmError> {
    let n = stacks.len();
    if n == 0 {
        return Err(IcmError::EmptyStacks);
    }
    if payouts.is_empty() {
        return Err(IcmError::EmptyPayouts);
    }
    if payouts.len() > n {
        return Err(IcmError::MorePayoutsThanPlayers);
    }
    for (idx, &stack) in stacks.iter().enumerate() {
        if stack <= 0 {
            return Err(IcmError::NonPositiveStack(idx));
        }
    }
    for (idx, &payout) in payouts.iter().enumerate() {
        if payout < 0 {
            return Err(IcmError::NegativePayout(idx));
        }
    }

    let m = payouts.len();
    let s: Vec<f64> = stacks.iter().map(|&c| c as f64).collect();
    let total: f64 = s.iter().sum();

    let mut subset_sum = vec![0.0f64; 1usize << n];
    for mask in 1..(1usize << n) {
        let low = mask.trailing_zeros() as usize;
        subset_sum[mask] = subset_sum[mask & (mask - 1)] + s[low];
    }

    let mut reach = vec![0.0f64; 1usize << n];
    reach[0] = 1.0;
    for mask in 1..(1usize << n) {
        let mut acc = 0.0;
        let mut rest = mask;
        while rest != 0 {
            let last = rest.trailing_zeros() as usize;
            let bit = 1usize << last;
            let prev = mask ^ bit;
            let denom = total - subset_sum[prev];
            acc += reach[prev] * s[last] / denom;
            rest &= rest - 1;
        }
        reach[mask] = acc;
    }

    let mut place_probs = vec![vec![0.0f64; n]; m];
    for place in 0..m {
        for player in 0..n {
            let player_bit = 1usize << player;
            let mut prob = 0.0;
            for mask in 0..(1usize << n) {
                if mask & player_bit != 0 {
                    continue;
                }
                if mask.count_ones() as usize != place {
                    continue;
                }
                prob += reach[mask] * s[player] / (total - subset_sum[mask]);
            }
            place_probs[place][player] = prob;
        }
    }

    let ev = (0..n)
        .map(|player| {
            (0..m)
                .map(|place| place_probs[place][player] * payouts[place] as f64)
                .sum()
        })
        .collect();

    Ok(IcmResult { ev, place_probs })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64, tol: f64) {
        assert!(
            (a - b).abs() < tol,
            "значения не совпадают: {a} vs {b} (diff {})",
            (a - b).abs()
        );
    }

    #[test]
    fn two_players_equal_stacks() {
        let r = icm_equity(&[1000, 1000], &[100, 50]).unwrap();
        assert_close(r.ev[0], 75.0, 1e-9);
        assert_close(r.ev[1], 75.0, 1e-9);
    }

    #[test]
    fn two_players_unequal_stacks_exact() {
        let r = icm_equity(&[2000, 1000], &[100, 50]).unwrap();
        let e0 = 100.0 * (2.0 / 3.0) + 50.0 * (1.0 / 3.0);
        let e1 = 100.0 * (1.0 / 3.0) + 50.0 * (2.0 / 3.0);
        assert_close(r.ev[0], e0, 1e-9);
        assert_close(r.ev[1], e1, 1e-9);
    }

    #[test]
    fn three_players_manual_calculation() {
        let r = icm_equity(&[1000, 500, 500], &[100, 50, 25]).unwrap();
        assert_close(r.place_probs[0][0], 0.5, 1e-9);
        assert_close(r.place_probs[1][0], 1.0 / 3.0, 1e-9);
        assert_close(r.place_probs[2][0], 1.0 / 6.0, 1e-9);
        let ev0 = 100.0 * 0.5 + 50.0 / 3.0 + 25.0 / 6.0;
        assert_close(r.ev[0], ev0, 1e-9);
        assert_close(r.ev[1], r.ev[2], 1e-9);
    }

    #[test]
    fn sum_of_ev_equals_prize_pool() {
        let stacks = [1000, 500, 500, 250, 100];
        let payouts = [100, 50, 25, 10, 5];
        let r = icm_equity(&stacks, &payouts).unwrap();
        let tev: f64 = r.ev.iter().sum();
        let pool: f64 = payouts.iter().map(|&p| p as f64).sum();
        assert_close(tev, pool, 1e-9);
    }

    #[test]
    fn single_payout_equal_stacks() {
        let r = icm_equity(&[1000, 1000, 1000], &[100]).unwrap();
        for ev in &r.ev {
            assert_close(*ev, 100.0 / 3.0, 1e-9);
        }
    }

    #[test]
    fn place_probs_sum_to_one() {
        let r = icm_equity(&[1000, 500, 500, 250], &[100, 50, 25, 10]).unwrap();
        for player in 0..4 {
            let sum: f64 = r.place_probs.iter().map(|p| p[player]).sum();
            assert_close(sum, 1.0, 1e-9);
        }
    }

    #[test]
    fn bigger_stack_never_has_lower_ev() {
        let r = icm_equity(&[3000, 1500, 700, 300], &[100, 60, 30, 10]).unwrap();
        assert!(r.ev[0] >= r.ev[1]);
        assert!(r.ev[1] >= r.ev[2]);
        assert!(r.ev[2] >= r.ev[3]);
    }

    #[test]
    fn invalid_inputs_rejected() {
        assert_eq!(icm_equity(&[], &[100]), Err(IcmError::EmptyStacks));
        assert_eq!(icm_equity(&[1000], &[]), Err(IcmError::EmptyPayouts));
        assert_eq!(
            icm_equity(&[1000], &[100, 50]),
            Err(IcmError::MorePayoutsThanPlayers)
        );
        assert_eq!(
            icm_equity(&[0, 1000], &[100]),
            Err(IcmError::NonPositiveStack(0))
        );
        assert_eq!(
            icm_equity(&[1000, 1000], &[100, -1]),
            Err(IcmError::NegativePayout(1))
        );
    }
}
