//! Эквити-уточнение тёрн/ривер-бакетов (T3.1, сессия 12, D-018).
//!
//! Механизм — зеркало флопа (D-017): те же шесть якорных матчапов из
//! общего ядра crate::equity (ANCHORS, инстанцирование, статистики,
//! квантили), эквити считается на канонических представителях классов
//! улиц (crate::street), z-агрегация — с весами кратностей классов,
//! группы — взвешенные квантили, бакет = бакет_v1 * группы + группа.
//!
//! Ривер: полный борд, эквити якоря дискретно — {0, 0.5, 1}; z-механика
//! работает, но сигнал грубее: это структурное свойство данных, не баг.
//!
//! Объём (от замеренной базы ~170k оценок/с): тёрн ~8.7M оценок ~50 c,
//! ривер ~1.6M ~10 c + генерация классов. Полные проходы — в #[ignore]
//! release-тестах и CLI; debug-тесты работают срезами.

use crate::equity::{
    aggregate_scores, anchor_stats_from_weights, quantile_groups_from_weights, AnchorStats,
    ANCHORS, ANCHOR_COUNT,
};
use crate::street::{street_classes, Street, StreetAbstraction, StreetClass};
use crate::{fnv1a64, granularity_tag, Granularity};
use std::collections::HashMap;

/// Тег домена fingerprint эквити-режима улиц: продолжает серию
/// equity-флопа (0xE0) и тегов v1 улиц (0x54/0x52).
const STREET_EQUITY_MODE_TAG: u8 = 0xE1;

fn compute_street_equities(
    classes: &[StreetClass],
) -> Result<Vec<[Option<f64>; ANCHOR_COUNT]>, String> {
    let mut equities = Vec::with_capacity(classes.len());
    for class in classes {
        let mut row = [None; ANCHOR_COUNT];
        for (a, anchor) in ANCHORS.iter().enumerate() {
            // rep класса улицы может нести паддинг 0xFF ([u8; 5] у тёрна):
            // ядру нужны ровно карты борда.
            let board = &class.rep[..class.street.card_count()];
            row[a] = crate::equity::anchor_equity_on_rep(anchor, board)?;
        }
        equities.push(row);
    }
    Ok(equities)
}

fn street_equity_fingerprint(
    street: Street,
    granularity: Granularity,
    equity_groups: usize,
    buckets: &[usize],
) -> u64 {
    let mut input: Vec<u8> = Vec::with_capacity(16 + buckets.len() * 8);
    input.push(STREET_EQUITY_MODE_TAG);
    input.push(street.tag());
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

/// Эквити-уточнённая абстракция улицы: тёрн или ривер.
pub struct StreetEquityAbstraction {
    street: Street,
    granularity: Granularity,
    equity_groups: usize,
    base: StreetAbstraction,
    equities: Vec<[Option<f64>; ANCHOR_COUNT]>,
    stats: Vec<AnchorStats>,
    scores: Vec<f64>,
    groups: Vec<usize>,
    buckets: Vec<usize>,
    weights: Vec<u64>,
    used: usize,
    fingerprint: u64,
}

impl StreetEquityAbstraction {
    /// Полный проход: тёрн ~минута, ривер ~15 c (release).
    pub fn new(
        street: Street,
        granularity: Granularity,
        equity_groups: usize,
    ) -> Result<Self, String> {
        let classes = street_classes(street);
        Self::from_class_slice(street, granularity, equity_groups, &classes)
    }

    fn from_class_slice(
        street: Street,
        granularity: Granularity,
        equity_groups: usize,
        classes: &[StreetClass],
    ) -> Result<Self, String> {
        if equity_groups < 2 {
            return Err(format!(
                "equity_groups must be at least 2, got {equity_groups}"
            ));
        }
        let equities = compute_street_equities(classes)?;
        Self::assemble(street, granularity, equity_groups, classes, equities)
    }

    fn assemble(
        street: Street,
        granularity: Granularity,
        equity_groups: usize,
        classes: &[StreetClass],
        equities: Vec<[Option<f64>; ANCHOR_COUNT]>,
    ) -> Result<Self, String> {
        if equity_groups < 2 {
            return Err(format!(
                "equity_groups must be at least 2, got {equity_groups}"
            ));
        }
        if classes.is_empty() {
            return Err("at least one street class is required".to_string());
        }
        if equities.len() != classes.len() {
            return Err(format!(
                "equity rows {} do not match class count {}",
                equities.len(),
                classes.len()
            ));
        }
        let base = StreetAbstraction::from_classes(street, granularity, classes.to_vec());
        let weights: Vec<u64> = classes.iter().map(|class| class.multiplicity).collect();
        let stats = anchor_stats_from_weights(&weights, &equities);
        let scores = aggregate_scores(&equities, &stats);
        let groups = quantile_groups_from_weights(&weights, &scores, equity_groups);
        let mut buckets = Vec::with_capacity(classes.len());
        for (i, _class) in classes.iter().enumerate() {
            let base_bucket = base
                .bucket_of(i)
                .ok_or_else(|| format!("{street} v1 bucket missing for class index {i}"))?;
            buckets.push(base_bucket * equity_groups + groups[i]);
        }
        let max_bucket = buckets.iter().copied().max().unwrap_or(0);
        let mut seen = vec![false; max_bucket + 1];
        for &bucket in &buckets {
            seen[bucket] = true;
        }
        let used = seen.iter().filter(|&&x| x).count();
        let fingerprint = street_equity_fingerprint(street, granularity, equity_groups, &buckets);
        Ok(StreetEquityAbstraction {
            street,
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

    pub fn street(&self) -> Street {
        self.street
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

    pub fn anchor_equity(&self, class_index: usize, anchor: usize) -> Option<f64> {
        self.equities
            .get(class_index)?
            .get(anchor)
            .copied()
            .flatten()
    }

    pub fn anchor_available(&self, class_index: usize, anchor: usize) -> Option<bool> {
        Some(self.equities.get(class_index)?.get(anchor)?.is_some())
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

    /// Бакет по конкретным картам борда (порядок любой). Осмыслен для
    /// абстракции, построенной по всем классам улицы (`new`).
    pub fn bucket_of_cards(&self, cards: &[u8]) -> Option<usize> {
        let n = self.street.card_count();
        if cards.len() != n {
            return None;
        }
        let mut seen = [false; 52];
        for &card in cards {
            if card >= 52 || seen[card as usize] {
                return None;
            }
            seen[card as usize] = true;
        }
        let index = self.base.class_index_of_cards(cards)?;
        self.buckets.get(index).copied()
    }

    /// Гистограмма по конкретным бордам: (бакет, Σ кратностей).
    pub fn board_histogram(&self) -> Vec<(usize, u64)> {
        let mut counts: HashMap<usize, u64> = HashMap::new();
        for (&bucket, &weight) in self.buckets.iter().zip(self.weights.iter()) {
            *counts.entry(bucket).or_insert(0) += weight;
        }
        let mut out: Vec<(usize, u64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// Гистограмма по классам: (бакет, число классов).
    pub fn histogram(&self) -> Vec<(usize, usize)> {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for &bucket in &self.buckets {
            *counts.entry(bucket).or_insert(0) += 1;
        }
        let mut out: Vec<(usize, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    pub fn total_boards(&self) -> u64 {
        self.weights.iter().sum()
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn c(rank: char, suit: char) -> u8 {
        let r = crate::RANK_CHARS
            .iter()
            .position(|&x| x as char == rank)
            .unwrap();
        let s = match suit {
            's' => 0,
            'h' => 1,
            'd' => 2,
            'c' => 3,
            _ => panic!("неизвестная масть"),
        };
        (r * 4 + s) as u8
    }

    fn turn_slice(step: usize) -> Vec<StreetClass> {
        let classes = street_classes(Street::Turn);
        (0..classes.len())
            .step_by(step)
            .map(|i| classes[i])
            .collect()
    }

    #[test]
    fn turn_equity_slice_is_structural() {
        let classes = turn_slice(400);
        let abstraction =
            StreetEquityAbstraction::from_class_slice(Street::Turn, Granularity::Fine, 4, &classes)
                .unwrap();
        // Абстракция построена по срезу: class_count — размер среза
        // (база v1 улиц собирается из переданных классов). Глобальное
        // число классов тёрна проверено в street::tests.
        assert_eq!(abstraction.class_count(), classes.len());
        assert_eq!(abstraction.anchor_count(), 6);
        let base =
            StreetAbstraction::from_classes(Street::Turn, Granularity::Fine, classes.clone());
        for (i, class) in classes.iter().enumerate() {
            let group = abstraction.group(i).unwrap();
            assert!(group < 4, "класс {}", class.label());
            assert_eq!(
                abstraction.bucket_of(i).unwrap(),
                base.bucket_of(i).unwrap() * 4 + group,
                "класс {}",
                class.label()
            );
        }
        // Инъективность: разные v1-бакеты не сливаются.
        for i in 0..classes.len() {
            for j in (i + 1)..classes.len() {
                if base.bucket_of(i) != base.bucket_of(j) {
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
                .board_histogram()
                .iter()
                .map(|&(_, boards)| boards)
                .sum::<u64>(),
            classes.iter().map(|class| class.multiplicity).sum()
        );
    }

    #[test]
    fn turn_equity_slice_is_deterministic() {
        let classes = turn_slice(1000);
        let first = StreetEquityAbstraction::from_class_slice(
            Street::Turn,
            Granularity::Medium,
            3,
            &classes,
        )
        .unwrap();
        let second = StreetEquityAbstraction::from_class_slice(
            Street::Turn,
            Granularity::Medium,
            3,
            &classes,
        )
        .unwrap();
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
    fn anchor_availability_on_quad_aces() {
        // Квады тузов: прямой борд, без генерации классов. Мертвы все якоря
        // с тузом в руках (AA, AKo-злодей, AKs-герой); выживает только
        // 99 vs TT — на борде только тузы.
        let quad_aces = [51u8, 50, 49, 48]; // As Ah Ad Ac
        for anchor_index in 0..5 {
            assert!(
                crate::equity::anchor_equity_on_rep(&ANCHORS[anchor_index], &quad_aces)
                    .unwrap()
                    .is_none(),
                "якорь {anchor_index} должен быть недоступен на AAAA"
            );
        }
        assert!(crate::equity::anchor_equity_on_rep(&ANCHORS[5], &quad_aces)
            .unwrap()
            .is_some());
    }

    #[test]
    fn river_anchor_equity_is_discrete() {
        // Дискретность — свойство полного борда, а не генератора классов:
        // прямые борды, без генерации 134k классов ривера.
        let boards: [[u8; 5]; 6] = [
            [
                c('A', 's'),
                c('K', 's'),
                c('Q', 's'),
                c('J', 's'),
                c('T', 's'),
            ],
            [
                c('A', 's'),
                c('K', 'd'),
                c('Q', 's'),
                c('J', 'c'),
                c('T', 'h'),
            ],
            [
                c('2', 's'),
                c('7', 'd'),
                c('9', 'c'),
                c('K', 'h'),
                c('3', 's'),
            ],
            [
                c('A', 's'),
                c('A', 'h'),
                c('A', 'd'),
                c('A', 'c'),
                c('K', 'd'),
            ],
            [
                c('K', 's'),
                c('K', 'h'),
                c('Q', 'd'),
                c('Q', 'c'),
                c('J', 's'),
            ],
            [
                c('8', 's'),
                c('7', 's'),
                c('6', 's'),
                c('5', 's'),
                c('4', 's'),
            ],
        ];
        for board in boards {
            for anchor in ANCHORS.iter() {
                if let Some(equity) = crate::equity::anchor_equity_on_rep(anchor, &board).unwrap() {
                    assert!(
                        equity == 0.0 || equity == 0.5 || equity == 1.0,
                        "{} на {:?}: эквити {equity}",
                        anchor.name,
                        board
                    );
                }
            }
        }
    }

    #[test]
    fn turn_group_counts_are_rejected() {
        // Пустой срез: валидация групп срабатывает до расчёта эквити.
        assert!(StreetEquityAbstraction::from_class_slice(
            Street::Turn,
            Granularity::Coarse,
            1,
            &[]
        )
        .is_err());
        assert!(StreetEquityAbstraction::from_class_slice(
            Street::Turn,
            Granularity::Coarse,
            0,
            &[]
        )
        .is_err());
    }

    #[test]
    fn bucket_of_cards_matches_v1_index() {
        let all = street_classes(Street::Turn);
        let board = [c('A', 's'), c('K', 'd'), c('Q', 's'), c('J', 'c')];
        // Полный v1 (без эквити) находит индекс класса борда.
        let lookup = StreetAbstraction::from_classes(Street::Turn, Granularity::Fine, all.clone());
        let index = lookup.class_index_of_cards(&board).unwrap();
        // Эквити-абстракция по малому срезу, содержащему этот класс:
        // debug-тест не тянет полный проход 16 432 классов.
        let slice: Vec<StreetClass> = (0..all.len())
            .filter(|&i| i == index || i % 5000 == 0)
            .map(|i| all[i])
            .collect();
        let abstraction =
            StreetEquityAbstraction::from_class_slice(Street::Turn, Granularity::Fine, 4, &slice)
                .unwrap();
        let bucket = abstraction.bucket_of_cards(&board).unwrap();
        let mut shuffled = board;
        shuffled.reverse();
        assert_eq!(abstraction.bucket_of_cards(&shuffled), Some(bucket));
        // Неверное число карт и дубликат отклоняются.
        assert!(abstraction
            .bucket_of_cards(&[c('A', 's'), c('K', 'd'), c('Q', 's')])
            .is_none());
        assert!(abstraction
            .bucket_of_cards(&[c('A', 's'), c('A', 's'), c('K', 'd'), c('Q', 's')])
            .is_none());
    }

    #[test]
    #[ignore] // полный проход: release, -- --ignored --nocapture street_equity
    fn street_equity_release_reference() {
        let started = std::time::Instant::now();
        for street in [Street::Turn, Street::River] {
            let classes = street_classes(street);
            let pass_started = std::time::Instant::now();
            // Эквити считается один раз; assemble дважды — детерминизм
            // сборки (как full_pass флопа).
            let equities = compute_street_equities(&classes).unwrap();
            println!(
                "{street}: эквити-проход {:?} ({} классов x {} якорей)",
                pass_started.elapsed(),
                classes.len(),
                ANCHOR_COUNT
            );
            for granularity in [Granularity::Coarse, Granularity::Medium, Granularity::Fine] {
                let first = StreetEquityAbstraction::assemble(
                    street,
                    granularity,
                    4,
                    &classes,
                    equities.clone(),
                )
                .unwrap();
                let second = StreetEquityAbstraction::assemble(
                    street,
                    granularity,
                    4,
                    &classes,
                    equities.clone(),
                )
                .unwrap();
                assert_eq!(first.fingerprint(), second.fingerprint());
                assert_eq!(first.histogram(), second.histogram());
                assert_eq!(first.board_histogram(), second.board_histogram());
                let base_used = StreetAbstraction::new(street, granularity).used_buckets();
                assert!(first.used_buckets() >= base_used);
                assert!(first.used_buckets() <= base_used * 4);
                println!(
                    "{street} {granularity}: classes={} used={} (v1={base_used}) fingerprint={:#018x}",
                    first.class_count(),
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
        }
        println!("total: {:?}", started.elapsed());
    }
}
