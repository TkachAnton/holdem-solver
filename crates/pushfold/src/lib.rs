//! HU пуш/фолд-решатель: fictitious play на 169 классах рук.
//!
//! Схема: эквити-матрица 169x169 (Монте-Карло, фикс. сид, полуквадрат
//! с зеркалированием) считается один раз; далее FP-итерации — чистая
//! арифметика по таблице. В дискретной игре чистой фиксированной точки
//! может не существовать, поэтому равновесие ищется в средних стратегиях
//! (частотах), а рекомендация — чистый BR против финальных средних.
//!
//! Экономика (эффективный стек S в bb, полный стек, блайнды 0.5/1.0):
//! * кнопка: фолд -0.5; пуш при фолде BB +1.0; пуш при колле S*(2eq-1);
//! * BB: фолд -1.0; колл S*(2eq-1), порог колла eq > (S-1)/2S.
//!
//! Качество — exploitability в bb: насколько чистый BR против финальных
//! средних лучше самих средних (см. D-012). Порог зависит от числа бордов
//! матрицы (шум MC): при 200 бордах на пару — 0.25bb.

use std::fmt;

const RANK_CHARS: &[u8; 13] = b"23456789TJQKA";

fn mk_card(rank: usize, suit: usize) -> u8 {
    (rank * 4 + suit) as u8
}
fn rank_of(card: u8) -> usize {
    (card >> 2) as usize
}
fn suit_of(card: u8) -> usize {
    (card & 3) as usize
}
fn rank_char(rank: usize) -> char {
    RANK_CHARS[rank] as char
}

#[derive(Debug, Clone, PartialEq)]
pub enum PushFoldError {
    InvalidStack(f64),
    InvalidIterations,
    DimensionMismatch,
    NoClasses,
}

impl fmt::Display for PushFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushFoldError::InvalidStack(s) => {
                write!(f, "stack {s} bb outside valid range 2.0..=50.0")
            }
            PushFoldError::InvalidIterations => write!(f, "iterations must be positive"),
            PushFoldError::DimensionMismatch => {
                write!(f, "classes and equity matrix dimensions do not match")
            }
            PushFoldError::NoClasses => write!(f, "no hand classes to solve"),
        }
    }
}

impl std::error::Error for PushFoldError {}

/// Детерминированный xorshift64* — без внешних зависимостей.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

/// Класс рук: индекс, метка, конкретные комбо, вес.
#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub index: usize,
    pub label: String,
    pub combos: Vec<(u8, u8)>,
    pub weight: usize,
}

/// Все 169 классов: пары 0..=12 (AA=0), suited 13..=90, offsuit 91..=168.
pub fn all_classes() -> Vec<ClassInfo> {
    let dummy = ClassInfo {
        index: 0,
        label: String::new(),
        combos: Vec::new(),
        weight: 0,
    };
    let mut out = vec![dummy; 169];
    for r in 0..13usize {
        let idx = 12 - r;
        let mut combos = Vec::new();
        for s1 in 0..4usize {
            for s2 in (s1 + 1)..4usize {
                combos.push((mk_card(r, s1), mk_card(r, s2)));
            }
        }
        let weight = combos.len();
        out[idx] = ClassInfo {
            index: idx,
            label: format!("{}{}", rank_char(r), rank_char(r)),
            combos,
            weight,
        };
    }
    for hi in 1..13usize {
        for lo in 0..hi {
            let pos = hi * (hi - 1) / 2 + lo;
            let s_idx = 13 + pos;
            let o_idx = 91 + pos;
            let mut suited = Vec::new();
            for s in 0..4usize {
                suited.push((mk_card(hi, s), mk_card(lo, s)));
            }
            out[s_idx] = ClassInfo {
                index: s_idx,
                label: format!("{}{}s", rank_char(hi), rank_char(lo)),
                combos: suited,
                weight: 4,
            };
            let mut offsuit = Vec::new();
            for s1 in 0..4usize {
                for s2 in 0..4usize {
                    if s1 != s2 {
                        offsuit.push((mk_card(hi, s1), mk_card(lo, s2)));
                    }
                }
            }
            let weight = offsuit.len();
            out[o_idx] = ClassInfo {
                index: o_idx,
                label: format!("{}{}o", rank_char(hi), rank_char(lo)),
                combos: offsuit,
                weight,
            };
        }
    }
    out
}

/// Индекс класса по двум картам (порядок любой).
pub fn class_index(a: u8, b: u8) -> usize {
    let ra = rank_of(a);
    let rb = rank_of(b);
    if ra == rb {
        return 12 - ra;
    }
    let (hi, lo) = if ra > rb { (ra, rb) } else { (rb, ra) };
    let pos = hi * (hi - 1) / 2 + lo;
    if suit_of(a) == suit_of(b) {
        13 + pos
    } else {
        91 + pos
    }
}

fn straight_high(mask: u16) -> Option<usize> {
    for lo in (0..9usize).rev() {
        if (mask >> lo) & 0b11111 == 0b11111 {
            return Some(lo + 4);
        }
    }
    if mask & (1 << 12) != 0 && mask & 0b1111 == 0b1111 {
        return Some(3);
    }
    None
}

fn top_ranks(mask: u16, n: usize, excl: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(n);
    for r in (0..13usize).rev() {
        if mask & (1 << r) != 0 && !excl.contains(&r) {
            out.push(r);
            if out.len() == n {
                break;
            }
        }
    }
    out
}

fn pack(cat: usize, rs: &[usize]) -> u32 {
    let mut v = (cat as u32) << 20;
    for i in 0..5usize {
        v |= (rs.get(i).copied().unwrap_or(0) as u32) << (16 - 4 * i);
    }
    v
}

/// Оценка 5..=7 карт: категория в старших битах, далее кикеры.
pub fn evaluate(cards: &[u8]) -> u32 {
    let mut rank_count = [0u8; 13];
    let mut suit_count = [0u8; 4];
    let mut suit_mask = [0u16; 4];
    for &c in cards {
        let r = rank_of(c);
        let s = suit_of(c);
        rank_count[r] += 1;
        suit_count[s] += 1;
        suit_mask[s] |= 1 << r;
    }
    let all: u16 = suit_mask[0] | suit_mask[1] | suit_mask[2] | suit_mask[3];
    let flush_suit = suit_count.iter().position(|&x| x >= 5);
    if let Some(fs) = flush_suit {
        if let Some(hi) = straight_high(suit_mask[fs]) {
            return pack(8, &[hi]);
        }
    }
    if let Some(q) = (0..13).rev().find(|&r| rank_count[r] == 4) {
        let kicker = (0..13)
            .rev()
            .find(|&r| r != q && rank_count[r] > 0)
            .unwrap_or(0);
        return pack(7, &[q, kicker]);
    }
    if let Some(t) = (0..13).rev().find(|&r| rank_count[r] == 3) {
        if let Some(p) = (0..13).rev().find(|&r| r != t && rank_count[r] >= 2) {
            return pack(6, &[t, p]);
        }
    }
    if let Some(fs) = flush_suit {
        let top = top_ranks(suit_mask[fs], 5, &[]);
        return pack(5, &top);
    }
    if let Some(hi) = straight_high(all) {
        return pack(4, &[hi]);
    }
    if let Some(t) = (0..13).rev().find(|&r| rank_count[r] == 3) {
        let ks = top_ranks(all, 2, &[t]);
        let mut rs = vec![t];
        rs.extend(ks);
        return pack(3, &rs);
    }
    let pairs: Vec<usize> = (0..13).rev().filter(|&r| rank_count[r] == 2).collect();
    if pairs.len() >= 2 {
        let ks = top_ranks(all, 1, &[pairs[0], pairs[1]]);
        let mut rs = vec![pairs[0], pairs[1]];
        rs.extend(ks);
        return pack(2, &rs);
    }
    if pairs.len() == 1 {
        let ks = top_ranks(all, 3, &[pairs[0]]);
        let mut rs = vec![pairs[0]];
        rs.extend(ks);
        return pack(1, &rs);
    }
    pack(0, &top_ranks(all, 5, &[]))
}

fn equity_mc(a: (u8, u8), b: (u8, u8), rng: &mut Rng, boards: usize) -> f64 {
    let mut deck: Vec<u8> = (0u8..52)
        .filter(|&c| c != a.0 && c != a.1 && c != b.0 && c != b.1)
        .collect();
    let mut wins = 0.0f64;
    for _ in 0..boards {
        for i in 0..5usize {
            let j = i + rng.below(48 - i);
            deck.swap(i, j);
        }
        let board = [deck[0], deck[1], deck[2], deck[3], deck[4]];
        let sa = evaluate(&[a.0, a.1, board[0], board[1], board[2], board[3], board[4]]);
        let sb = evaluate(&[b.0, b.1, board[0], board[1], board[2], board[3], board[4]]);
        if sa > sb {
            wins += 1.0;
        } else if sa == sb {
            wins += 0.5;
        }
    }
    wins / boards as f64
}

/// Эквити руки против равномерно случайного оппонента.
pub fn equity_vs_random(hand: (u8, u8), boards: usize, seed: u64) -> f64 {
    let mut rng = Rng::new(seed);
    let others: Vec<u8> = (0u8..52).filter(|&c| c != hand.0 && c != hand.1).collect();
    let mut wins = 0.0f64;
    for _ in 0..boards {
        let i = rng.below(50);
        let mut j = rng.below(49);
        if j >= i {
            j += 1;
        }
        wins += equity_mc(hand, (others[i], others[j]), &mut rng, 1);
    }
    wins / boards as f64
}

/// Эквити-матрица классов: e[i][j] — эквити i против j.
/// Полуквадрат + зеркалирование: e[j][i] = 1 - e[i][j]; диагональ 0.5.
pub struct EquityMatrix {
    pub n: usize,
    pub e: Vec<f64>,
}

impl EquityMatrix {
    pub fn compute(
        classes: &[ClassInfo],
        boards_per_matchup: usize,
        seed: u64,
    ) -> Result<Self, PushFoldError> {
        let n = classes.len();
        if n == 0 {
            return Err(PushFoldError::NoClasses);
        }
        let mut e = vec![0.5f64; n * n];
        let mut rng = Rng::new(seed);
        for i in 0..n {
            for j in (i + 1)..n {
                let mut acc = 0.0f64;
                let mut used = 0usize;
                let mut attempts = 0usize;
                while used < 3 && attempts < 64 {
                    attempts += 1;
                    let ca = classes[i].combos[rng.below(classes[i].combos.len())];
                    let cb = classes[j].combos[rng.below(classes[j].combos.len())];
                    if ca.0 == cb.0 || ca.0 == cb.1 || ca.1 == cb.0 || ca.1 == cb.1 {
                        continue;
                    }
                    let b = (boards_per_matchup / 3).max(1);
                    acc += equity_mc(ca, cb, &mut rng, b);
                    used += 1;
                }
                let v = if used > 0 { acc / used as f64 } else { 0.5 };
                e[i * n + j] = v;
                e[j * n + i] = 1.0 - v;
            }
        }
        Ok(EquityMatrix { n, e })
    }

    pub fn at(&self, i: usize, j: usize) -> f64 {
        self.e[i * self.n + j]
    }
}

/// Эквити класса hero против смешанного диапазона freq (веса = weight*freq).
fn eq_vs_freq(matrix: &EquityMatrix, classes: &[ClassInfo], hero: usize, freq: &[f64]) -> f64 {
    let mut s = 0.0f64;
    let mut w = 0.0f64;
    for j in 0..classes.len() {
        let wj = classes[j].weight as f64 * freq[j];
        s += wj * matrix.at(hero, j);
        w += wj;
    }
    if w > 0.0 {
        s / w
    } else {
        0.5
    }
}

/// Результат FP-решения.
#[derive(Debug, Clone)]
pub struct PushFoldResult {
    pub stack_bb: f64,
    /// Чистая рекомендация: BR против финальных средних оппонента.
    pub button_push: Vec<bool>,
    /// Средняя частота пуша FP (анализ миксов; 0..1).
    pub button_freq: Vec<f64>,
    /// EV пуша каждой руки против финальной средней BB.
    pub button_ev: Vec<f64>,
    pub bb_call: Vec<bool>,
    pub bb_freq: Vec<f64>,
    /// EV колла каждой руки против финальной средней кнопки.
    pub bb_call_ev: Vec<f64>,
    pub push_combos: usize,
    pub call_combos: usize,
    pub iterations: usize,
    pub stable: bool,
    /// Exploitability в bb: max по игрокам (u(BR против средних) - u(средних)).
    pub exploitability_bb: f64,
}

/// Fictitious play: каждый раунд обе стороны берут чистый BR против
/// накопленных средних оппонента, средние обновляются кумулятивно.
/// Рекомендация — финальный чистый BR; качество — exploitability.
pub fn solve_hu(
    stack_bb: f64,
    matrix: &EquityMatrix,
    classes: &[ClassInfo],
    max_iterations: usize,
) -> Result<PushFoldResult, PushFoldError> {
    if !(2.0..=50.0).contains(&stack_bb) {
        return Err(PushFoldError::InvalidStack(stack_bb));
    }
    if max_iterations == 0 {
        return Err(PushFoldError::InvalidIterations);
    }
    let n = classes.len();
    if n == 0 || matrix.n != n {
        return Err(PushFoldError::DimensionMismatch);
    }
    let total: f64 = classes.iter().map(|c| c.weight as f64).sum();

    let mut btn_freq = vec![1.0f64; n];
    let mut bb_freq = vec![1.0f64; n];
    let mut prev_br_push: Option<Vec<bool>> = None;
    let mut prev_br_call: Option<Vec<bool>> = None;
    let mut stable = false;
    let mut done = 0usize;

    for iter in 0..max_iterations {
        done = iter + 1;
        // Чистые BR против текущих средних.
        let call_w: f64 = (0..n).map(|j| classes[j].weight as f64 * bb_freq[j]).sum();
        let fold_frac = 1.0 - call_w / total;
        let mut br_push = vec![false; n];
        for i in 0..n {
            let eqc = eq_vs_freq(matrix, classes, i, &bb_freq);
            let ev = fold_frac + (1.0 - fold_frac) * stack_bb * (2.0 * eqc - 1.0);
            br_push[i] = ev > -0.5;
        }
        let push_w: f64 = (0..n).map(|i| classes[i].weight as f64 * btn_freq[i]).sum();
        let mut br_call = vec![false; n];
        if push_w > 0.0 {
            for j in 0..n {
                let eq = eq_vs_freq(matrix, classes, j, &btn_freq);
                let ev = stack_bb * (2.0 * eq - 1.0);
                br_call[j] = ev > -1.0;
            }
        }
        // Стабильность: BR не менялись два раунда подряд.
        if let (Some(pp), Some(pc)) = (&prev_br_push, &prev_br_call) {
            if *pp == br_push && *pc == br_call {
                stable = true;
                // Всё равно обновляем средние этим BR — они совпадают с прошлыми.
            }
        }
        prev_br_push = Some(br_push.clone());
        prev_br_call = Some(br_call.clone());
        // Кумулятивное обновление средних.
        let t = iter as f64 + 1.0;
        for i in 0..n {
            btn_freq[i] = (btn_freq[i] * (t - 1.0) + br_push[i] as u8 as f64) / t;
        }
        for j in 0..n {
            bb_freq[j] = (bb_freq[j] * (t - 1.0) + br_call[j] as u8 as f64) / t;
        }
        if stable {
            break;
        }
    }

    // Финальные EV и рекомендация — чистый BR против финальных средних.
    let call_w: f64 = (0..n).map(|j| classes[j].weight as f64 * bb_freq[j]).sum();
    let fold_frac = 1.0 - call_w / total;
    let mut button_push = vec![false; n];
    let mut button_ev = vec![0.0f64; n];
    for i in 0..n {
        let eqc = eq_vs_freq(matrix, classes, i, &bb_freq);
        let ev = fold_frac + (1.0 - fold_frac) * stack_bb * (2.0 * eqc - 1.0);
        button_ev[i] = ev;
        button_push[i] = ev > -0.5;
    }
    let mut bb_call = vec![false; n];
    let mut bb_call_ev = vec![0.0f64; n];
    for j in 0..n {
        let eq = eq_vs_freq(matrix, classes, j, &btn_freq);
        let ev = stack_bb * (2.0 * eq - 1.0);
        bb_call_ev[j] = ev;
        bb_call[j] = ev > -1.0;
    }

    // Exploitability: u(BR) - u(средняя) для каждого игрока, берём max.
    let u_btn_avg: f64 = (0..n)
        .map(|i| {
            classes[i].weight as f64 / total
                * (btn_freq[i] * button_ev[i] + (1.0 - btn_freq[i]) * (-0.5))
        })
        .sum();
    let u_btn_br: f64 = (0..n)
        .map(|i| classes[i].weight as f64 / total * button_ev[i].max(-0.5))
        .sum();
    let u_bb_avg: f64 = (0..n)
        .map(|j| {
            classes[j].weight as f64 / total
                * (bb_freq[j] * bb_call_ev[j] + (1.0 - bb_freq[j]) * (-1.0))
        })
        .sum();
    let u_bb_br: f64 = (0..n)
        .map(|j| classes[j].weight as f64 / total * bb_call_ev[j].max(-1.0))
        .sum();
    let exploitability_bb = (u_btn_br - u_btn_avg).max(u_bb_br - u_bb_avg);

    let push_combos = (0..n)
        .filter(|&i| button_push[i])
        .map(|i| classes[i].weight)
        .sum();
    let call_combos = (0..n)
        .filter(|&j| bb_call[j])
        .map(|j| classes[j].weight)
        .sum();

    Ok(PushFoldResult {
        stack_bb,
        button_push,
        button_freq: btn_freq,
        button_ev,
        bb_call,
        bb_freq: bb_freq,
        bb_call_ev,
        push_combos,
        call_combos,
        iterations: done,
        stable,
        exploitability_bb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(rank: char, suit: char) -> u8 {
        let r = RANK_CHARS.iter().position(|&x| x as char == rank).unwrap();
        let s = match suit {
            's' => 0,
            'h' => 1,
            'd' => 2,
            'c' => 3,
            _ => panic!("unknown suit"),
        };
        mk_card(r, s)
    }

    #[test]
    fn evaluator_orders_categories() {
        let sf = evaluate(&[
            c('A', 'h'),
            c('K', 'h'),
            c('Q', 'h'),
            c('J', 'h'),
            c('T', 'h'),
        ]);
        let quads = evaluate(&[
            c('2', 's'),
            c('2', 'h'),
            c('2', 'd'),
            c('2', 'c'),
            c('A', 'd'),
        ]);
        let fh = evaluate(&[
            c('K', 'h'),
            c('K', 'd'),
            c('K', 'c'),
            c('3', 's'),
            c('3', 'd'),
        ]);
        let flush = evaluate(&[
            c('2', 'h'),
            c('5', 'h'),
            c('7', 'h'),
            c('9', 'h'),
            c('J', 'h'),
        ]);
        let straight = evaluate(&[
            c('5', 's'),
            c('6', 'd'),
            c('7', 'c'),
            c('8', 'h'),
            c('9', 's'),
        ]);
        let trips = evaluate(&[
            c('7', 's'),
            c('7', 'd'),
            c('7', 'c'),
            c('A', 'h'),
            c('2', 'd'),
        ]);
        let two_pair = evaluate(&[
            c('A', 's'),
            c('A', 'd'),
            c('K', 'h'),
            c('K', 'c'),
            c('5', 's'),
        ]);
        let pair = evaluate(&[
            c('A', 's'),
            c('A', 'd'),
            c('K', 'h'),
            c('6', 'c'),
            c('5', 's'),
        ]);
        let high = evaluate(&[
            c('A', 's'),
            c('K', 'd'),
            c('7', 'c'),
            c('5', 'h'),
            c('2', 's'),
        ]);
        assert!(sf > quads);
        assert!(quads > fh);
        assert!(fh > flush);
        assert!(flush > straight);
        assert!(straight > trips);
        assert!(trips > two_pair);
        assert!(two_pair > pair);
        assert!(pair > high);
    }

    #[test]
    fn evaluator_handles_wheel_and_seven_cards() {
        let wheel = evaluate(&[
            c('A', 'h'),
            c('2', 's'),
            c('3', 'd'),
            c('4', 'c'),
            c('5', 'h'),
        ]);
        let six = evaluate(&[
            c('2', 's'),
            c('3', 'd'),
            c('4', 'c'),
            c('5', 'h'),
            c('6', 's'),
        ]);
        assert_eq!(wheel >> 20, 4);
        assert!(six > wheel);
        let royal7 = evaluate(&[
            c('A', 'h'),
            c('K', 'h'),
            c('Q', 'h'),
            c('J', 'h'),
            c('T', 'h'),
            c('2', 's'),
            c('3', 'd'),
        ]);
        let quads7 = evaluate(&[
            c('2', 's'),
            c('2', 'h'),
            c('2', 'd'),
            c('2', 'c'),
            c('A', 'd'),
            c('K', 's'),
            c('Q', 's'),
        ]);
        assert_eq!(royal7 >> 20, 8);
        assert!(royal7 > quads7);
    }

    #[test]
    fn classes_are_complete_and_indexable() {
        let classes = all_classes();
        assert_eq!(classes.len(), 169);
        let total: usize = classes.iter().map(|x| x.weight).sum();
        assert_eq!(total, 1326);
        let mut labels: Vec<&str> = classes.iter().map(|x| x.label.as_str()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), 169);
        assert_eq!(classes[0].label, "AA");
        assert_eq!(classes[0].weight, 6);
        assert_eq!(classes[90].label, "AKs");
        assert_eq!(classes[168].label, "AKo");
        assert_eq!(classes[168].weight, 12);
        for info in &classes {
            for &(a, b) in &info.combos {
                assert_eq!(class_index(a, b), info.index);
                assert_eq!(class_index(b, a), info.index);
            }
        }
    }

    #[test]
    fn equity_anchors_vs_random() {
        let aa = (c('A', 'h'), c('A', 'd'));
        let eq_aa = equity_vs_random(aa, 600, 101);
        assert!(eq_aa > 0.78 && eq_aa < 0.92, "AA vs random: {eq_aa}");
        let trash = (c('7', 'c'), c('2', 'd'));
        let eq_t = equity_vs_random(trash, 600, 102);
        assert!(eq_t > 0.27 && eq_t < 0.43, "72o vs random: {eq_t}");
    }

    #[test]
    fn matrix_is_antisymmetric() {
        let classes = all_classes();
        let m = EquityMatrix::compute(&classes, 9, 0x5EED_0002).unwrap();
        for i in 0..classes.len() {
            assert_eq!(m.at(i, i), 0.5);
            for j in (i + 1)..classes.len() {
                let sum = m.at(i, j) + m.at(j, i);
                assert!(
                    (sum - 1.0).abs() < 1e-9,
                    "антисимметрия нарушена: {i} vs {j}"
                );
            }
        }
    }

    #[test]
    fn solve_10bb_fictitious_play() {
        let classes = all_classes();
        let m = EquityMatrix::compute(&classes, 200, 0x5EED_0001).unwrap();
        let res = solve_hu(10.0, &m, &classes, 60).unwrap();
        assert!(
            res.exploitability_bb <= 0.25,
            "exploitability = {}",
            res.exploitability_bb
        );
        assert!(res.button_push[0], "AA пушится на 10bb");
        assert!(
            !res.button_push[class_index(c('7', 'c'), c('2', 'd'))],
            "72o фолдится на 10bb"
        );
        assert!(res.bb_call[0], "BB коллирует AA");
        assert!(
            !res.bb_call[class_index(c('3', 'c'), c('2', 'd'))],
            "BB выкидывает 32o"
        );
        assert!(
            res.push_combos >= 380 && res.push_combos <= 820,
            "push% на 10bb вне коридора катастрофы: {}",
            res.push_combos
        );
        assert!(
            res.call_combos >= 190 && res.call_combos <= 560,
            "call% на 10bb вне коридора катастрофы: {}",
            res.call_combos
        );
    }

    #[test]
    fn deeper_stacks_push_less() {
        let classes = all_classes();
        let m = EquityMatrix::compute(&classes, 60, 0x5EED_0003).unwrap();
        let r5 = solve_hu(5.0, &m, &classes, 60).unwrap();
        let r10 = solve_hu(10.0, &m, &classes, 60).unwrap();
        let r25 = solve_hu(25.0, &m, &classes, 60).unwrap();
        assert!(
            r5.push_combos > r10.push_combos,
            "5bb пушит не шире 10bb: {} vs {}",
            r5.push_combos,
            r10.push_combos
        );
        assert!(
            r10.push_combos > r25.push_combos,
            "10bb пушит не шире 25bb: {} vs {}",
            r10.push_combos,
            r25.push_combos
        );
    }

    #[test]
    fn invalid_inputs_rejected() {
        let classes = all_classes();
        let m = EquityMatrix::compute(&classes, 9, 0x5EED_0004).unwrap();
        assert!(solve_hu(1.5, &m, &classes, 5).is_err());
        assert!(solve_hu(0.0, &m, &classes, 5).is_err());
        assert!(solve_hu(10.0, &m, &classes, 0).is_err());
        let empty: Vec<ClassInfo> = Vec::new();
        assert!(solve_hu(10.0, &m, &empty, 5).is_err());
    }
    #[test]
    fn exact_equity_fixtures() {
        // D-013: доверенные точные якоря, а не память агента.
        // AA vs KK ~ 81.9%, AKs vs QQ ~ 46.2%, AKo vs 22 ~ 47.6%,
        // AA vs random ~ 85.2%, 32o vs random ~ 32.3% (худшая рука).
        let mut rng = Rng::new(0xF17E_0001);
        let aa_kk = equity_mc(
            (c('A', 'h'), c('A', 'd')),
            (c('K', 'h'), c('K', 's')),
            &mut rng,
            40_000,
        );
        assert!((aa_kk - 0.819).abs() < 0.025, "AA vs KK: {aa_kk}");
        let aks_qq = equity_mc(
            (c('A', 's'), c('K', 's')),
            (c('Q', 'h'), c('Q', 'd')),
            &mut rng,
            40_000,
        );
        assert!((aks_qq - 0.462).abs() < 0.025, "AKs vs QQ: {aks_qq}");
        let ako_22 = equity_mc(
            (c('A', 'h'), c('K', 'd')),
            (c('2', 'c'), c('2', 's')),
            &mut rng,
            40_000,
        );
        assert!((ako_22 - 0.476).abs() < 0.025, "AKo vs 22: {ako_22}");
        let aa_rand = equity_vs_random((c('A', 'h'), c('A', 'd')), 4_000, 0xF17E_0002);
        assert!((aa_rand - 0.852).abs() < 0.03, "AA vs random: {aa_rand}");
        let low_rand = equity_vs_random((c('3', 'c'), c('2', 'd')), 4_000, 0xF17E_0003);
        assert!((low_rand - 0.323).abs() < 0.03, "32o vs random: {low_rand}");
    }
}
