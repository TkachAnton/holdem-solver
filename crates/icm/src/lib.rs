//! Точный ICM (Independent Chip Model) калькулятор по Malmuth-Harville.
//!
//! Модель: вероятность каждого порядка финишей пропорциональна произведению
//! долей стеков выбывающих. Вероятности мест вычисляются динамическим
//! программированием по подмножествам игроков: O(2^n * n).
//!
//! Поверх базового расчёта строятся bubble-метрики (T1.2):
//! * all-in bubble factor — цена риска против конкретного оппонента;
//! * маргинальный bubble factor — цена следующей фишки без вылетов.
//!
//! Осознанные ограничения модели (см. docs/DECISIONS.md):
//! * равный скилл всех игроков;
//! * future game value не учитывается: одна раздача — горизонт модели.

use std::fmt;

/// Целочисленные фишки — единая валюта домена (см. holdem-domain).
pub type Chips = i64;

/// Ошибка расчёта ICM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcmError {
    EmptyStacks,
    EmptyPayouts,
    MorePayoutsThanPlayers,
    NonPositiveStack(usize),
    NegativePayout(usize),
    InvalidPlayerIndex(usize),
    SamePlayer,
    InvalidDelta,
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
            IcmError::InvalidPlayerIndex(idx) => {
                write!(f, "player index {} is out of range", idx)
            }
            IcmError::SamePlayer => write!(f, "hero and villain must be different players"),
            IcmError::InvalidDelta => {
                write!(f, "delta must be positive and below both players' stacks")
            }
        }
    }
}

impl std::error::Error for IcmError {}

/// Результат расчёта ICM.
#[derive(Debug, Clone, PartialEq)]
pub struct IcmResult {
    /// Ожидаемая денежная ценность каждого игрока.
    pub ev: Vec<f64>,
    /// Вероятности мест: place_probs[place][player].
    /// Место 0 — первое. Размер равен числу оплачиваемых мест.
    pub place_probs: Vec<Vec<f64>>,
}

/// Вычисляет $EV стеков по модели Malmuth-Harville.
///
/// Вероятность порядка финишей (i1, i2, ..., in) равна
/// (s_i1/T) * (s_i2/(T - s_i1)) * ... — доля стека каждого выбывающего
/// от остатка турнира. DP по подмножествам даёт маргинальные вероятности
/// мест без перебора n! перестановок.
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

    // subset_sum[mask] — суммарный стек игроков в маске.
    let mut subset_sum = vec![0.0f64; 1usize << n];
    for mask in 1..(1usize << n) {
        let low = mask.trailing_zeros() as usize;
        subset_sum[mask] = subset_sum[mask & (mask - 1)] + s[low];
    }

    // reach[mask] — вероятность того, что игроки из маски заняли первые
    // |mask| мест в каком-то порядке. reach[0] = 1.
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

    // Маргинальная вероятность места: игрок p занимает место `place`,
    // если до него выбыло любое подмножество из `place` игроков без него.
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

// ---------------------------------------------------------------------------
// Bubble-метрики (T1.2)
// ---------------------------------------------------------------------------

/// Приз за последнее место при `player_count` игроках.
/// Если оплачиваемых мест меньше, чем игроков, — ноль.
fn last_place_payout(payouts: &[Chips], player_count: usize) -> f64 {
    if payouts.len() >= player_count {
        payouts[player_count - 1] as f64
    } else {
        0.0
    }
}

/// $EV героя в состоянии после перехода, где не более одного игрока
/// мог вылететь (нулевой стек). Вылетевший занимает последнее место,
/// оставшиеся делят усечённые призы.
fn ev_after_transition(
    stacks_after: &[Chips],
    payouts: &[Chips],
    hero: usize,
) -> Result<f64, IcmError> {
    let n = stacks_after.len();
    if stacks_after[hero] == 0 {
        return Ok(last_place_payout(payouts, n));
    }
    let mut alive: Vec<Chips> = Vec::with_capacity(n);
    let mut hero_alive_index = 0usize;
    for (idx, &stack) in stacks_after.iter().enumerate() {
        if stack > 0 {
            if idx == hero {
                hero_alive_index = alive.len();
            }
            alive.push(stack);
        }
    }
    let paid = payouts.len().min(alive.len());
    let result = icm_equity(&alive, &payouts[..paid])?;
    Ok(result.ev[hero_alive_index])
}

fn validate_pair(n: usize, hero: usize, villain: usize) -> Result<(), IcmError> {
    if hero >= n {
        return Err(IcmError::InvalidPlayerIndex(hero));
    }
    if villain >= n {
        return Err(IcmError::InvalidPlayerIndex(villain));
    }
    if hero == villain {
        return Err(IcmError::SamePlayer);
    }
    Ok(())
}

/// Результат all-in bubble factor для пары игроков.
#[derive(Debug, Clone, PartialEq)]
pub struct AllInBubble {
    /// Эффективный стек пары.
    pub effective_stack: Chips,
    /// $EV героя сейчас.
    pub current_ev: f64,
    /// $EV героя, если он выиграл all-in на эффективный стек.
    pub win_ev: f64,
    /// $EV героя, если он проиграл all-in (при вылете — приз за последнее место).
    pub lose_ev: f64,
    /// loss / gain: во сколько раз потерянная фишка дороже выигранной.
    /// Больше единицы — риск дороже награды (баббл-эффект).
    pub bubble_factor: f64,
}

/// All-in bubble factor героя против конкретного оппонента.
///
/// Сценарий: hero и villain идут all-in на эффективный стек
/// min(s_hero, s_villain); прочие стеки не меняются. Выигрыш и проигрыш
/// считаются точно, включая вылеты: игрок с нулевым стеком после перехода
/// занимает последнее место (приз за него — см. D-009).
pub fn all_in_bubble_factor(
    stacks: &[Chips],
    payouts: &[Chips],
    hero: usize,
    villain: usize,
) -> Result<AllInBubble, IcmError> {
    let current = icm_equity(stacks, payouts)?;
    validate_pair(stacks.len(), hero, villain)?;

    let eff = stacks[hero].min(stacks[villain]);
    let mut win = stacks.to_vec();
    win[hero] += eff;
    win[villain] -= eff;
    let mut lose = stacks.to_vec();
    lose[hero] -= eff;
    lose[villain] += eff;

    let win_ev = ev_after_transition(&win, payouts, hero)?;
    let lose_ev = ev_after_transition(&lose, payouts, hero)?;
    let gain = win_ev - current.ev[hero];
    let loss = current.ev[hero] - lose_ev;
    let bubble_factor = if gain > 0.0 {
        loss / gain
    } else {
        f64::INFINITY
    };

    Ok(AllInBubble {
        effective_stack: eff,
        current_ev: current.ev[hero],
        win_ev,
        lose_ev,
        bubble_factor,
    })
}

/// Маргинальный bubble factor: цена следующей фишки без вылетов.
///
/// Переносит `delta` фишек от villain к hero и обратно (никто не вылетает:
/// delta строго меньше обоих стеков) и возвращает отношение
/// потери $EV к выигрышу $EV на этот перенос.
pub fn marginal_bubble_factor(
    stacks: &[Chips],
    payouts: &[Chips],
    hero: usize,
    villain: usize,
    delta: Chips,
) -> Result<f64, IcmError> {
    let current = icm_equity(stacks, payouts)?;
    validate_pair(stacks.len(), hero, villain)?;

    let cap = stacks[hero].min(stacks[villain]);
    if delta <= 0 || delta >= cap {
        return Err(IcmError::InvalidDelta);
    }

    let mut win = stacks.to_vec();
    win[hero] += delta;
    win[villain] -= delta;
    let mut lose = stacks.to_vec();
    lose[hero] -= delta;
    lose[villain] += delta;

    let win_ev = ev_after_transition(&win, payouts, hero)?;
    let lose_ev = ev_after_transition(&lose, payouts, hero)?;
    let gain = win_ev - current.ev[hero];
    let loss = current.ev[hero] - lose_ev;
    if gain > 0.0 {
        Ok(loss / gain)
    } else {
        Ok(f64::INFINITY)
    }
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

    // ----- T1.2: bubble factor -----

    #[test]
    fn hu_bubble_factor_is_one() {
        // ICM в хедз-апе линейна: потерянная фишка равна выигранной.
        let b = all_in_bubble_factor(&[1000, 1000], &[100, 50], 0, 1).unwrap();
        assert_close(b.current_ev, 75.0, 1e-9);
        assert_close(b.win_ev, 100.0, 1e-9);
        assert_close(b.lose_ev, 50.0, 1e-9);
        assert_close(b.bubble_factor, 1.0, 1e-12);
    }

    #[test]
    fn bubble_three_equal_stacks_reference() {
        // Классика: баббл, три равных стека, призы 650/350.
        // current = 1000/3; win = 550; lose = 0; BF = (1000/3)/(650/3) = 20/13.
        let b = all_in_bubble_factor(&[1000, 1000, 1000], &[650, 350], 0, 1).unwrap();
        assert_close(b.current_ev, 1000.0 / 3.0, 1e-9);
        assert_close(b.win_ev, 550.0, 1e-9);
        assert_close(b.lose_ev, 0.0, 1e-9);
        assert_close(b.bubble_factor, 20.0 / 13.0, 1e-9);
    }

    #[test]
    fn hero_covers_villain_manual() {
        // Hero крупнее villain: проигрыш не вылетает, риск дешевле.
        let b = all_in_bubble_factor(&[3000, 1000, 1000], &[100, 50, 25], 0, 1).unwrap();
        assert_eq!(b.effective_stack, 1000);
        assert_close(b.current_ev, 77.5, 1e-9);
        assert_close(b.win_ev, 90.0, 1e-9);
        assert_close(b.lose_ev, 385.0 / 6.0, 1e-9);
        assert_close(b.bubble_factor, 16.0 / 15.0, 1e-9);
    }

    #[test]
    fn shorter_hero_busts_on_loss() {
        // Hero короче: проигрыш — вылет на 3-е место, приз 25 (m == n).
        let b = all_in_bubble_factor(&[1000, 3000, 1000], &[100, 50, 25], 0, 1).unwrap();
        assert_close(b.current_ev, 48.75, 1e-9);
        assert_close(b.win_ev, 385.0 / 6.0, 1e-9);
        assert_close(b.lose_ev, 25.0, 1e-9);
        assert_close(b.bubble_factor, 57.0 / 37.0, 1e-9);
    }

    #[test]
    fn marginal_bubble_factor_positive_on_bubble() {
        // Вогнутость ICM: потеря фиксированной доли стека дороже выигрыша.
        let bf = marginal_bubble_factor(&[1000, 1000, 1000], &[650, 350], 0, 1, 10).unwrap();
        assert!(
            bf > 1.0,
            "маргинальный BF на баббле должен быть > 1, got {bf}"
        );
        assert!(bf.is_finite());
    }

    #[test]
    fn marginal_bubble_factor_is_one_hu() {
        let bf = marginal_bubble_factor(&[1000, 1000], &[100, 50], 0, 1, 10).unwrap();
        assert_close(bf, 1.0, 1e-9);
    }

    #[test]
    fn bubble_apis_reject_bad_inputs() {
        assert_eq!(
            all_in_bubble_factor(&[1000, 1000], &[100], 0, 0),
            Err(IcmError::SamePlayer)
        );
        assert_eq!(
            all_in_bubble_factor(&[1000, 1000], &[100], 0, 2),
            Err(IcmError::InvalidPlayerIndex(2))
        );
        assert_eq!(
            marginal_bubble_factor(&[1000, 1000], &[100], 0, 1, 0),
            Err(IcmError::InvalidDelta)
        );
        assert_eq!(
            marginal_bubble_factor(&[1000, 1000], &[100], 0, 1, 1000),
            Err(IcmError::InvalidDelta)
        );
    }
}
