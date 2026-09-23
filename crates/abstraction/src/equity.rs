//! Эквити-уточнение флоп-бакетов (T3.1 v2, сессия 11, D-017) и общее
//! якорное ядро для эквити-слоя улиц (сессия 12, D-018).
//!
//! Поверх детерминированных feature-бакетов v1 строится эквити-слой: для
//! каждого класса флопа считается точное эквити героя в якорных матчапах
//! (перебор всех C(45,2) = 990 продолжений через `exact_profile_equity`),
//! z-нормализуется по распределению всех классов (веса — кратности флопов)
//! и усредняется по доступным якорям. Финальный бакет:
//! `бакет_v1 * эквити_группы + группа`, группа — взвешенный квантиль
//! агрегатного скора. RNG и сидов нет: детерминизм структурный.
//!
//! Якорное ядро (ANCHORS, инстанцирование рук, взвешенные статистики,
//! квантили) опубликовано как pub(crate) и переиспользуется эквити-слоем
//! тёрна и ривера (crate::street_equity): одни и те же шесть якорей на
//! всех улицах — свойство D-017; расхождение копиями исключено by design.
//!
//! Стоимость полного прохода: 1755 классов x 6 якорей x 990 x 2 оценки ~
//! 21 млн оценок — ~123 с в release (замер сессии 11, AI_LOG); полный
//! прогон вынесен в `#[ignore]` release-тест. v1 (`lib.rs`) не меняется:
//! эквити-режим живёт поверх него.

use crate::{
    flop_class_of_cards, flop_classes, fnv1a64, granularity_tag, FlopAbstraction, FlopClass,
    Granularity, SuitPattern,
};
use holdem_equity::exact_profile_equity;
use holdem_ranges::Combo;
use std::collections::HashMap;

/// Префикс домена fingerprint эквити-режима: отличается от тегов v1 (1..3).
const EQUITY_MODE_TAG: u8 = 0xE0;

/// Рука якоря: паттерн и ранги (0='2' .. 12='A', как во всём крейте).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorHand {
    Pair(usize),
    Suited(usize, usize),
    Offsuit(usize, usize),
}

/// Якорный матчуп «герой против злодея» на представителе класса.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Anchor {
    pub(crate) name: &'static str,
    pub(crate) role: &'static str,
    pub(crate) hero: AnchorHand,
    pub(crate) villain: AnchorHand,
}

/// Шесть якорей — шесть покерных реальностей (D-017): доминация старших
/// пар, пара против оверкарт, коннекторы-дро, suited-бродвей, малая пара,
/// средний пояс. Роли не дублируют друг друга: дубль смещал бы агрегат
/// двойным весом, а не добавлял информацию.
pub(crate) const ANCHORS: [Anchor; 6] = [
    Anchor {
        name: "AA vs KK",
        role: "доминация старших пар: сеты, коллизии A/K",
        hero: AnchorHand::Pair(12),
        villain: AnchorHand::Pair(11),
    },
    Anchor {
        name: "JJ vs AKo",
        role: "пара против двух оверкарт",
        hero: AnchorHand::Pair(9),
        villain: AnchorHand::Offsuit(12, 11),
    },
    Anchor {
        name: "87s vs AKo",
        role: "коннекторы: дро на связных бордах",
        hero: AnchorHand::Suited(6, 5),
        villain: AnchorHand::Offsuit(12, 11),
    },
    Anchor {
        name: "AKs vs QQ",
        role: "suited-бродвей: флеш-потенциал и оверкарты",
        hero: AnchorHand::Suited(12, 11),
        villain: AnchorHand::Pair(10),
    },
    Anchor {
        name: "22 vs AKo",
        role: "малая пара: сет-майнинг",
        hero: AnchorHand::Pair(0),
        villain: AnchorHand::Offsuit(12, 11),
    },
    Anchor {
        name: "99 vs TT",
        role: "пары среднего пояса: стрит- и сет-зона",
        hero: AnchorHand::Pair(7),
        villain: AnchorHand::Pair(8),
    },
];

pub(crate) const ANCHOR_COUNT: usize = ANCHORS.len();

/// Число конкретных флопов в классе: trips/monotone = C(4,3) = 4;
/// paired и two-tone = C(4,2)*2 = 12; rainbow = 4*3*2 = 24.
/// Сумма по всем 1755 классам равна C(52,3) — проверяется тестом.
fn class_multiplicity(class: &FlopClass) -> u64 {
    match class.pattern {
        SuitPattern::Trips | SuitPattern::Monotone => 4,
        SuitPattern::PairedRainbow
        | SuitPattern::PairedTwoTone
        | SuitPattern::TwoToneHighMid
        | SuitPattern::TwoToneHighLow
        | SuitPattern::TwoToneMidLow => 12,
        SuitPattern::Rainbow => 24,
    }
}

fn used_cards(rep: &[u8]) -> [bool; 52] {
    let mut used = [false; 52];
    for &card in rep {
        used[card as usize] = true;
    }
    used
}

/// Детерминированное инстанцирование руки относительно занятых карт.
/// Пара: первые две свободные карты ранга (на флопе недоступна только на
/// трипсе своего ранга; на 4/5-картных бордах блокировок больше —
/// недоступность возвращает None и обрабатывается агрегатом). Suited:
/// масть борда в приоритете (флеш-сигнал), затем остальные — на флопе
/// инстанцируема всегда, на улицах может быть недоступна. Offsuit:
/// первые свободные карты разных мастей.
fn instantiate(hand: AnchorHand, used: &[bool; 52], rep: &[u8]) -> Option<[u8; 2]> {
    match hand {
        AnchorHand::Pair(rank) => {
            let mut cards = [0u8; 2];
            let mut found = 0;
            for suit in 0..4 {
                let card = (rank * 4 + suit) as u8;
                if !used[card as usize] {
                    cards[found] = card;
                    found += 1;
                    if found == 2 {
                        return Some(cards);
                    }
                }
            }
            None
        }
        AnchorHand::Suited(hi, lo) => {
            let mut order: Vec<usize> = rep.iter().map(|&c| (c & 3) as usize).collect();
            order.sort_unstable();
            order.dedup();
            for suit in 0..4 {
                if !order.contains(&suit) {
                    order.push(suit);
                }
            }
            for &suit in &order {
                let hi_card = (hi * 4 + suit) as u8;
                let lo_card = (lo * 4 + suit) as u8;
                if !used[hi_card as usize] && !used[lo_card as usize] {
                    return Some([hi_card, lo_card]);
                }
            }
            None
        }
        AnchorHand::Offsuit(hi, lo) => {
            for hi_suit in 0..4 {
                let hi_card = (hi * 4 + hi_suit) as u8;
                if used[hi_card as usize] {
                    continue;
                }
                for lo_suit in 0..4 {
                    if lo_suit == hi_suit {
                        continue;
                    }
                    let lo_card = (lo * 4 + lo_suit) as u8;
                    if !used[lo_card as usize] {
                        return Some([hi_card, lo_card]);
                    }
                }
            }
            None
        }
    }
}

/// Руки якоря на борде-представителе (3..5 карт). None — якорь не
/// инстанцируется. `rep` — ровно карты борда, без паддинга.
/// инстанцируется: на флопе это только пара ранга трипса, на улицах
/// набор блокировок богаче (квады и т.п.) и проверяется кодом, а не
/// перенесённым правилом.
pub(crate) fn anchor_hands_on_rep(anchor: &Anchor, rep: &[u8]) -> Option<([u8; 2], [u8; 2])> {
    let board_used = used_cards(rep);
    let hero = instantiate(anchor.hero, &board_used, rep)?;
    let mut all_used = board_used;
    all_used[hero[0] as usize] = true;
    all_used[hero[1] as usize] = true;
    let villain = instantiate(anchor.villain, &all_used, rep)?;
    Some((hero, villain))
}

#[cfg(test)]
fn anchor_hands_on(anchor: &Anchor, class: &FlopClass) -> Option<([u8; 2], [u8; 2])> {
    anchor_hands_on_rep(anchor, &class.rep)
}

/// Точное эквити героя якоря на борде-представителе (shares[0]).
/// `rep` — ровно карты борда: без паддинга 0xFF из [u8; 5] классов улиц
/// Ривер даёт дискретные {0, 0.5, 1} — свойство полного борда.
pub(crate) fn anchor_equity_on_rep(anchor: &Anchor, rep: &[u8]) -> Result<Option<f64>, String> {
    let Some((hero, villain)) = anchor_hands_on_rep(anchor, rep) else {
        return Ok(None);
    };
    let hands = [
        Combo::new(hero[0], hero[1])?,
        Combo::new(villain[0], villain[1])?,
    ];
    let board = rep.to_vec();
    let result = exact_profile_equity(&hands, &[0, 1], &board)?;
    Ok(Some(result.shares[0]))
}

#[cfg(test)]
fn anchor_equity_on_class(anchor: &Anchor, class: &FlopClass) -> Result<Option<f64>, String> {
    anchor_equity_on_rep(anchor, &class.rep)
}

fn compute_anchor_equities(
    classes: &[FlopClass],
) -> Result<Vec<[Option<f64>; ANCHOR_COUNT]>, String> {
    let mut equities = Vec::with_capacity(classes.len());
    for class in classes {
        let mut row = [None; ANCHOR_COUNT];
        for (a, anchor) in ANCHORS.iter().enumerate() {
            row[a] = anchor_equity_on_rep(anchor, &class.rep)?;
        }
        equities.push(row);
    }
    Ok(equities)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AnchorStats {
    pub(crate) mean: f64,
    pub(crate) std: f64,
    pub(crate) weight: f64,
    pub(crate) available: usize,
}

/// Взвешенные среднее и стандартное отклонение каждого якоря по
/// переданным эквити. Ядро, общее для всех улиц: веса передаются явно
/// (флоп — кратности классов флопов, улицы — multiplicity классов
/// бордов).
pub(crate) fn anchor_stats_from_weights(
    weights: &[u64],
    equities: &[[Option<f64>; ANCHOR_COUNT]],
) -> Vec<AnchorStats> {
    debug_assert_eq!(weights.len(), equities.len());
    let mut sums = vec![0.0; ANCHOR_COUNT];
    let mut totals = vec![0.0; ANCHOR_COUNT];
    let mut available = vec![0usize; ANCHOR_COUNT];
    for (i, row) in equities.iter().enumerate() {
        let weight = weights[i] as f64;
        for a in 0..ANCHOR_COUNT {
            if let Some(equity) = row[a] {
                sums[a] += weight * equity;
                totals[a] += weight;
                available[a] += 1;
            }
        }
    }
    let mut stats: Vec<AnchorStats> = (0..ANCHOR_COUNT)
        .map(|a| AnchorStats {
            mean: if totals[a] > 0.0 {
                sums[a] / totals[a]
            } else {
                0.0
            },
            std: 0.0,
            weight: totals[a],
            available: available[a],
        })
        .collect();
    for (i, row) in equities.iter().enumerate() {
        let weight = weights[i] as f64;
        for a in 0..ANCHOR_COUNT {
            if let Some(equity) = row[a] {
                let delta = equity - stats[a].mean;
                stats[a].std += weight * delta * delta;
            }
        }
    }
    for stat in &mut stats {
        if stat.weight > 0.0 {
            stat.std = (stat.std / stat.weight).sqrt();
        }
    }
    stats
}

/// Агрегатный скор класса: среднее z-оценок доступных якорей.
pub(crate) fn aggregate_scores(
    equities: &[[Option<f64>; ANCHOR_COUNT]],
    stats: &[AnchorStats],
) -> Vec<f64> {
    equities
        .iter()
        .map(|row| {
            let mut sum = 0.0;
            let mut count = 0usize;
            for a in 0..ANCHOR_COUNT {
                if let Some(equity) = row[a] {
                    let stat = &stats[a];
                    if stat.weight > 0.0 && stat.std > 1e-12 {
                        sum += (equity - stat.mean) / stat.std;
                        count += 1;
                    }
                }
            }
            if count == 0 {
                0.0
            } else {
                sum / count as f64
            }
        })
        .collect()
}

/// Взвешенные квантильные группы: классы сортируются по скору, граница
/// группы — доля суммарного веса. Порогов из памяти нет (D-013): границы
/// выводятся из фактического распределения. Ядро с явными весами.
pub(crate) fn quantile_groups_from_weights(
    weights: &[u64],
    scores: &[f64],
    groups: usize,
) -> Vec<usize> {
    debug_assert_eq!(weights.len(), scores.len());
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&a, &b| scores[a].total_cmp(&scores[b]).then(a.cmp(&b)));
    let total: f64 = weights.iter().sum::<u64>() as f64;
    let mut out = vec![0usize; weights.len()];
    let mut cumulative = 0.0f64;
    for &index in &order {
        let group = ((cumulative / total) * groups as f64).floor() as usize;
        out[index] = group.min(groups - 1);
        cumulative += weights[index] as f64;
    }
    out
}

fn equity_fingerprint(granularity: Granularity, equity_groups: usize, buckets: &[usize]) -> u64 {
    let mut input: Vec<u8> = Vec::with_capacity(16 + buckets.len() * 8);
    input.push(EQUITY_MODE_TAG);
    input.push(granularity_tag(granularity));
    input.extend_from_slice(&(equity_groups as u64).to_le_bytes());
    input.push(ANCHOR_COUNT as u8);
    for anchor in &ANCHORS {
        input.extend_from_slice(anchor.name.as_bytes());
        input.push(0xFF);
    }
    for &bucket in buckets {
        input.extend_from_slice(&(bucket as u64).to_le_bytes());
    }
    fnv1a64(&input)
}

/// Эквити-уточнённая абстракция флопа (T3.1 v2, D-017). Строится поверх
/// нетронутого v1: `бакет = бакет_v1 * equity_groups + группа`. Индексы
/// классов — позиции в переданном наборе; для `new` это полная нумерация
/// `flop_classes()`.
pub struct FlopEquityAbstraction {
    granularity: Granularity,
    equity_groups: usize,
    base: FlopAbstraction,
    equities: Vec<[Option<f64>; ANCHOR_COUNT]>,
    stats: Vec<AnchorStats>,
    scores: Vec<f64>,
    groups: Vec<usize>,
    buckets: Vec<usize>,
    weights: Vec<u64>,
    used: usize,
    fingerprint: u64,
}

impl FlopEquityAbstraction {
    /// Полный проход по всем 1755 классам: секунды-минуты в release.
    pub fn new(granularity: Granularity, equity_groups: usize) -> Result<Self, String> {
        let classes = flop_classes();
        Self::from_class_slice(granularity, equity_groups, &classes)
    }

    fn from_class_slice(
        granularity: Granularity,
        equity_groups: usize,
        classes: &[FlopClass],
    ) -> Result<Self, String> {
        if equity_groups < 2 {
            return Err(format!(
                "equity_groups must be at least 2, got {equity_groups}"
            ));
        }
        let equities = compute_anchor_equities(classes)?;
        Self::assemble(granularity, equity_groups, classes, equities)
    }

    fn assemble(
        granularity: Granularity,
        equity_groups: usize,
        classes: &[FlopClass],
        equities: Vec<[Option<f64>; ANCHOR_COUNT]>,
    ) -> Result<Self, String> {
        if equity_groups < 2 {
            return Err(format!(
                "equity_groups must be at least 2, got {equity_groups}"
            ));
        }
        if classes.is_empty() {
            return Err("at least one flop class is required".to_string());
        }
        if equities.len() != classes.len() {
            return Err(format!(
                "equity rows {} do not match class count {}",
                equities.len(),
                classes.len()
            ));
        }
        let base = FlopAbstraction::new(granularity);
        let weights: Vec<u64> = classes.iter().map(class_multiplicity).collect();
        let stats = anchor_stats_from_weights(&weights, &equities);
        let scores = aggregate_scores(&equities, &stats);
        let groups = quantile_groups_from_weights(&weights, &scores, equity_groups);
        let mut buckets = Vec::with_capacity(classes.len());
        for (i, class) in classes.iter().enumerate() {
            let base_bucket = base
                .bucket_of(class.index)
                .ok_or_else(|| format!("v1 bucket missing for class index {}", class.index))?;
            buckets.push(base_bucket * equity_groups + groups[i]);
        }
        let max_bucket = buckets.iter().copied().max().unwrap_or(0);
        let mut seen = vec![false; max_bucket + 1];
        for &bucket in &buckets {
            seen[bucket] = true;
        }
        let used = seen.iter().filter(|&&x| x).count();
        let fingerprint = equity_fingerprint(granularity, equity_groups, &buckets);
        Ok(FlopEquityAbstraction {
            granularity,
            equity_groups,
            base,
            equities,
            stats,
            scores,
            groups,
            buckets,
            weights,
            used,
            fingerprint,
        })
    }

    pub fn granularity(&self) -> Granularity {
        self.granularity
    }

    pub fn equity_groups(&self) -> usize {
        self.equity_groups
    }

    pub fn class_count(&self) -> usize {
        self.base.class_count()
    }

    pub fn used_buckets(&self) -> usize {
        self.used
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn base_fingerprint(&self) -> u64 {
        self.base.fingerprint()
    }

    pub fn anchor_count(&self) -> usize {
        ANCHOR_COUNT
    }

    pub fn anchor_name(&self, anchor: usize) -> Option<&'static str> {
        ANCHORS.get(anchor).map(|a| a.name)
    }

    pub fn anchor_role(&self, anchor: usize) -> Option<&'static str> {
        ANCHORS.get(anchor).map(|a| a.role)
    }

    pub fn anchor_available_classes(&self, anchor: usize) -> Option<usize> {
        self.stats.get(anchor).map(|s| s.available)
    }

    pub fn anchor_mean(&self, anchor: usize) -> Option<f64> {
        self.stats.get(anchor).map(|s| s.mean)
    }

    pub fn anchor_std(&self, anchor: usize) -> Option<f64> {
        self.stats.get(anchor).map(|s| s.std)
    }

    pub fn anchor_available(&self, class_index: usize, anchor: usize) -> Option<bool> {
        Some(self.equities.get(class_index)?.get(anchor)?.is_some())
    }

    pub fn anchor_equity(&self, class_index: usize, anchor: usize) -> Option<f64> {
        self.equities
            .get(class_index)?
            .get(anchor)
            .copied()
            .flatten()
    }

    pub fn score(&self, class_index: usize) -> Option<f64> {
        self.scores.get(class_index).copied()
    }

    pub fn group(&self, class_index: usize) -> Option<usize> {
        self.groups.get(class_index).copied()
    }

    pub fn bucket_of(&self, class_index: usize) -> Option<usize> {
        self.buckets.get(class_index).copied()
    }

    /// Бакет по трём конкретным картам флопа. Осмыслен для абстракции,
    /// построенной по всем классам (`new`).
    pub fn bucket_of_cards(&self, cards: &[u8]) -> Option<usize> {
        let class = flop_class_of_cards(cards)?;
        self.buckets.get(class.index).copied()
    }

    /// Гистограмма по классам, как в v1: (бакет, число классов).
    pub fn histogram(&self) -> Vec<(usize, usize)> {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for &bucket in &self.buckets {
            *counts.entry(bucket).or_insert(0) += 1;
        }
        let mut out: Vec<(usize, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// Гистограмма по реальным флопам: (бакет, число флопов из C(52,3)).
    pub fn flop_histogram(&self) -> Vec<(usize, u64)> {
        let mut counts: HashMap<usize, u64> = HashMap::new();
        for (&bucket, &weight) in self.buckets.iter().zip(self.weights.iter()) {
            *counts.entry(bucket).or_insert(0) += weight;
        }
        let mut out: Vec<(usize, u64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    pub fn total_flops(&self) -> u64 {
        self.weights.iter().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_ids_match_holdem_cards() {
        // Схема рангов/мастей крейта (rank*4+suit) обязана совпадать со
        // схемой holdem-cards: иначе якоря молча измеряют другие матчапы.
        let ranks: [(char, usize); 13] = [
            ('2', 0),
            ('3', 1),
            ('4', 2),
            ('5', 3),
            ('6', 4),
            ('7', 5),
            ('8', 6),
            ('9', 7),
            ('T', 8),
            ('J', 9),
            ('Q', 10),
            ('K', 11),
            ('A', 12),
        ];
        let suits: [(char, usize); 4] = [('s', 0), ('h', 1), ('d', 2), ('c', 3)];
        for (rank_char, rank) in ranks {
            for (suit_char, suit) in suits {
                assert_eq!(
                    holdem_cards::card_code(rank_char, suit_char),
                    Some((rank * 4 + suit) as u8),
                    "несовпадение кодировки {rank_char}{suit_char}"
                );
            }
        }
    }

    #[test]
    fn multiplicity_sums_to_all_flops() {
        let classes = flop_classes();
        let total: u64 = classes.iter().map(class_multiplicity).sum();
        assert_eq!(total, 52 * 51 * 50 / 6); // C(52,3) = 22100
    }

    #[test]
    fn anchor_availability_matches_trips_rule() {
        let classes = flop_classes();
        let mut counts = vec![0usize; ANCHOR_COUNT];
        for class in &classes {
            let trips_rank = (class.ranks[0] == class.ranks[1] && class.ranks[1] == class.ranks[2])
                .then_some(class.ranks[0]);
            for (a, anchor) in ANCHORS.iter().enumerate() {
                let available = anchor_hands_on(anchor, class).is_some();
                let pair_ranks = [anchor.hero, anchor.villain]
                    .into_iter()
                    .filter_map(|hand| match hand {
                        AnchorHand::Pair(rank) => Some(rank),
                        _ => None,
                    })
                    .collect::<Vec<usize>>();
                let blocked = pair_ranks.iter().any(|&rank| Some(rank) == trips_rank);
                assert_eq!(
                    available,
                    !blocked,
                    "{} на {}: доступность {available}",
                    anchor.name,
                    class.label()
                );
                if available {
                    counts[a] += 1;
                }
            }
        }
        // Из правила трипсов: якорь 0 теряет AAA и KKK, якорь 1 — JJJ,
        // якорь 3 — QQQ, якорь 4 — 222, якорь 5 — 999 и TTT, якорь 2 не
        // теряет ничего. Итого из 1755: [1753, 1754, 1755, 1754, 1754, 1753].
        assert_eq!(counts, vec![1753, 1754, 1755, 1754, 1754, 1753]);
    }

    #[test]
    fn anchor_equity_is_a_share() {
        let classes = flop_classes();
        // Срез секций без блокировок якорей: трипс 555 (4), парный 322 (13),
        // монотонный AKQ (329); блокирующие трипсы 222/AAA — ниже.
        for &index in &[4usize, 13, 329] {
            let class = &classes[index];
            for anchor in &ANCHORS {
                let equity = anchor_equity_on_class(anchor, class)
                    .unwrap_or_else(|error| panic!("{}: {error}", anchor.name))
                    .unwrap_or_else(|| panic!("{} недоступен на {}", anchor.name, class.label()));
                assert!(
                    (0.0..=1.0).contains(&equity),
                    "{} на {}: эквити {equity}",
                    anchor.name,
                    class.label()
                );
            }
        }
        // AAA мёртв только для AA vs KK: не осталось двух тузов.
        let aaa = &classes[12];
        assert!(anchor_equity_on_class(&ANCHORS[0], aaa).unwrap().is_none());
        for anchor in &ANCHORS[1..] {
            assert!(anchor_equity_on_class(anchor, aaa).unwrap().is_some());
        }
        // Трипс 222 мёртв для 22 vs AKo (осталась одна двойка) и жив для остальных.
        let deuces = &classes[0];
        assert!(anchor_equity_on_class(&ANCHORS[4], deuces)
            .unwrap()
            .is_none());
        for (a, anchor) in ANCHORS.iter().enumerate() {
            if a != 4 {
                assert!(anchor_equity_on_class(anchor, deuces).unwrap().is_some());
            }
        }
    }

    fn strided_classes(step: usize) -> Vec<FlopClass> {
        let classes = flop_classes();
        (0..classes.len())
            .step_by(step)
            .map(|index| classes[index].clone())
            .collect()
    }

    #[test]
    fn equity_pipeline_slice_is_structural() {
        let classes = strided_classes(100);
        let abstraction =
            FlopEquityAbstraction::from_class_slice(Granularity::Fine, 4, &classes).unwrap();
        assert_eq!(abstraction.class_count(), 1755);
        assert_eq!(abstraction.anchor_count(), 6);
        let base = FlopAbstraction::new(Granularity::Fine);
        for (i, class) in classes.iter().enumerate() {
            let group = abstraction.group(i).unwrap();
            assert!(group < 4, "класс {}", class.label());
            assert_eq!(
                abstraction.bucket_of(i).unwrap(),
                base.bucket_of(class.index).unwrap() * 4 + group,
                "класс {}",
                class.label()
            );
        }
        // Разные v1-бакеты не сливаются: bucket = v1 * G + g инъективен по v1.
        // Монотонность used на полном наборе классов — в release-тесте.
        for i in 0..classes.len() {
            for j in (i + 1)..classes.len() {
                if base.bucket_of(classes[i].index) != base.bucket_of(classes[j].index) {
                    assert_ne!(
                        abstraction.bucket_of(i).unwrap(),
                        abstraction.bucket_of(j).unwrap(),
                        "слияние v1-бакетов: {} и {}",
                        classes[i].label(),
                        classes[j].label()
                    );
                }
            }
        }
        assert!(abstraction.used_buckets() <= base.used_buckets() * 4);
        assert_eq!(
            abstraction
                .flop_histogram()
                .iter()
                .map(|&(_, flops)| flops)
                .sum::<u64>(),
            classes.iter().map(class_multiplicity).sum()
        );
    }

    #[test]
    fn equity_pipeline_slice_is_deterministic() {
        let classes = strided_classes(300);
        let first =
            FlopEquityAbstraction::from_class_slice(Granularity::Medium, 3, &classes).unwrap();
        let second =
            FlopEquityAbstraction::from_class_slice(Granularity::Medium, 3, &classes).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_eq!(first.used_buckets(), second.used_buckets());
        assert_eq!(first.histogram(), second.histogram());
        for index in 0..classes.len() {
            assert_eq!(first.bucket_of(index), second.bucket_of(index));
            assert_eq!(first.group(index), second.group(index));
            assert_eq!(first.score(index), second.score(index));
        }
    }

    #[test]
    fn tiny_group_counts_are_rejected() {
        assert!(FlopEquityAbstraction::new(Granularity::Coarse, 0).is_err());
        assert!(FlopEquityAbstraction::new(Granularity::Coarse, 1).is_err());
    }

    #[test]
    fn v1_fingerprints_are_pinned() {
        // Регрессионные пины из зафиксированных прогонов (не из памяти):
        // fine — смок сессии 10 (AI_LOG); coarse — base_fingerprint
        // equity-смока сессии 11 (AI_LOG). Medium нигде не зафиксирован —
        // печатается для закрепления фактом (следующий коммит).
        let coarse = FlopAbstraction::new(Granularity::Coarse);
        let medium = FlopAbstraction::new(Granularity::Medium);
        let fine = FlopAbstraction::new(Granularity::Fine);
        assert_eq!(
            format!("{:#018x}", coarse.fingerprint()),
            "0xdce5e3a95475acb5"
        );
        assert_eq!(
            format!("{:#018x}", medium.fingerprint()),
            "0x97ad8be3d6155afe"
        );
        assert_eq!(
            format!("{:#018x}", fine.fingerprint()),
            "0x8fb68ef5db8dbe5b"
        );
    }

    #[test]
    #[ignore] // полный проход: cargo test -p holdem-solver-abstraction --release -- --ignored --nocapture
    fn full_pass_release_reference() {
        let started = std::time::Instant::now();
        let classes = flop_classes();
        let equities = compute_anchor_equities(&classes).unwrap();
        println!(
            "equity pass: {:?} ({} классов x {} якорей)",
            started.elapsed(),
            classes.len(),
            ANCHOR_COUNT
        );
        for granularity in [Granularity::Coarse, Granularity::Medium, Granularity::Fine] {
            let first = FlopEquityAbstraction::assemble(granularity, 4, &classes, equities.clone())
                .unwrap();
            let second =
                FlopEquityAbstraction::assemble(granularity, 4, &classes, equities.clone())
                    .unwrap();
            assert_eq!(first.fingerprint(), second.fingerprint());
            assert_eq!(first.histogram(), second.histogram());
            assert_eq!(first.flop_histogram(), second.flop_histogram());
            // Регрессионные пины (AI_LOG, сессия 11): гейт рефакторинга
            // D-018 — ядро pub(crate) не должно менять ни бит математики.
            let expected = match granularity {
                Granularity::Coarse => 0x4d35_c0dc_dc9f_0cf8_u64,
                Granularity::Medium => 0x2744_bcce_b148_074a_u64,
                Granularity::Fine => 0x796a_6d09_a515_ba0f_u64,
            };
            assert_eq!(
                first.fingerprint(),
                expected,
                "флоп-эквити fingerprint дрейфовал после рефакторинга"
            );
            let base_used = FlopAbstraction::new(granularity).used_buckets();
            assert!(first.used_buckets() >= base_used);
            assert!(first.used_buckets() <= base_used * 4);
            println!(
                "{granularity}: used={} (v1={base_used}), fingerprint={:#018x}",
                first.used_buckets(),
                first.fingerprint()
            );
            for a in 0..ANCHOR_COUNT {
                println!(
                    "  {} [{}]: available={} mean={:.4} std={:.4}",
                    ANCHORS[a].name,
                    ANCHORS[a].role,
                    first.anchor_available_classes(a).unwrap(),
                    first.anchor_mean(a).unwrap(),
                    first.anchor_std(a).unwrap(),
                );
            }
        }
        assert_eq!(
            FlopEquityAbstraction::assemble(Granularity::Medium, 4, &classes, equities)
                .unwrap()
                .total_flops(),
            22100
        );
        println!("total: {:?}", started.elapsed());
    }
}
