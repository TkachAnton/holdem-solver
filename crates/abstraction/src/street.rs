//! Канонические классы бордов улиц: тёрн (4 карты) и ривер (5 карт).
//!
//! T3.1, продолжение (сессия 12, D-018). Улица — плоское множество карт
//! без порядка прихода: переходы флоп -> тёрн -> ривер — задача T3.2.
//! Два борда в одном классе, если один получается из другого
//! перестановкой мастей. Канонический ключ — минимум по 24
//! перестановкам мастей от множества карт, отсортированного по
//! убыванию значения карты; при совпадающих рангах наивный подсчёт
//! «паттернов мастей по разбиениям позиций» завышает классы (квады —
//! один борд, а не 24), поэтому классы генерирует код: ранговые
//! кортежи × назначения мастей с дедупом по ключу; кратность класса —
//! размер орбиты. Корректность — инвариантом Σ кратностей = C(52, n)
//! и независимой brute-force сверкой всех бордов (тесты).
//!
//! v1 — структурные бакеты: семейство ранговых мультиплетов × форма
//! мастей × ранговые группы Granularity (top/mid/span — общие хелперы
//! lib.rs, как у флопа). v2 — эквити-уточнение поверх (следующий кусок
//! сессии 12, механизм D-017).
//!
//! Кодировка карт — как во всём крейте: rank = card >> 2, suit = card & 3,
//! карта = rank * 4 + suit; связь с holdem-cards проверена тестом
//! equity::tests::card_ids_match_holdem_cards.

use crate::{fnv1a64, granularity_tag, mid_group, top_group, Granularity};
use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

const SUIT_CHARS: &[u8; 4] = b"shdc";

/// Улица постфлопа: тёрн (4 карты борда) или ривер (5 карт).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Street {
    Turn,
    River,
}

impl Street {
    pub fn card_count(self) -> usize {
        match self {
            Street::Turn => 4,
            Street::River => 5,
        }
    }

    /// Байтовый тег для fingerprint: отличается от тегов v1 (1..3) и
    /// эквити-режима флопа (0xE0).
    pub(crate) fn tag(self) -> u8 {
        match self {
            Street::Turn => 0x54,  // 'T'
            Street::River => 0x52, // 'R'
        }
    }
}

impl fmt::Display for Street {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Street::Turn => write!(f, "turn"),
            Street::River => write!(f, "river"),
        }
    }
}

/// Канонический класс борда улицы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreetClass {
    pub index: usize,
    pub street: Street,
    /// Канонический представитель: карты по убыванию значения,
    /// хвост — 0xFF.
    pub rep: [u8; 5],
    /// Число конкретных бордов в классе (размер орбиты под 24
    /// перестановками мастей).
    pub multiplicity: u64,
}

impl StreetClass {
    /// Ранги по убыванию.
    pub fn ranks(&self) -> Vec<usize> {
        self.rep[..self.street.card_count()]
            .iter()
            .map(|&card| (card >> 2) as usize)
            .collect()
    }

    pub fn label(&self) -> String {
        let mut out = String::new();
        for &card in &self.rep[..self.street.card_count()] {
            out.push(crate::RANK_CHARS[(card >> 2) as usize] as char);
            out.push(SUIT_CHARS[(card & 3) as usize] as char);
        }
        out
    }
}

fn suit_permutations() -> &'static [[usize; 4]; 24] {
    static PERMS: OnceLock<[[usize; 4]; 24]> = OnceLock::new();
    PERMS.get_or_init(|| {
        let mut out = [[0usize; 4]; 24];
        let mut k = 0;
        for a in 0..4 {
            for b in 0..4 {
                if b == a {
                    continue;
                }
                for c in 0..4 {
                    if c == a || c == b {
                        continue;
                    }
                    for d in 0..4 {
                        if d == a || d == b || d == c {
                            continue;
                        }
                        out[k] = [a, b, c, d];
                        k += 1;
                    }
                }
            }
        }
        out
    })
}

/// Канонический ключ множества карт борда: лексикографический минимум
/// по 24 перестановкам мастей (карты в ключе — по убыванию значения,
/// хвост 0xFF). Два борда в одном классе ⇔ равные ключи.
fn canonical_key(cards: &[u8]) -> [u8; 5] {
    debug_assert!(!cards.is_empty() && cards.len() <= 5);
    let mut best = [0xFFu8; 5];
    for perm in suit_permutations() {
        let mut mapped = [0xFFu8; 5];
        for (slot, &card) in cards.iter().enumerate() {
            let rank = (card >> 2) as usize;
            let suit = (card & 3) as usize;
            mapped[slot] = (rank * 4 + perm[suit]) as u8;
        }
        // сортируем только занятые слоты: паддинг 0xFF остаётся в хвосте
        mapped[..cards.len()].sort_unstable_by(|a, b| b.cmp(a));
        if mapped < best {
            best = mapped;
        }
    }
    best
}

/// Все ранговые кортежи длины n по невозрастанию; ранг встречается не
/// более 4 раз (мастей всего 4). Порядок лексикографический по
/// убыванию: детерминирован.
fn rank_tuples_desc(n: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut stack: Vec<usize> = Vec::with_capacity(n);
    build_rank_tuples(12, n, &mut stack, &mut out);
    out
}

fn build_rank_tuples(
    max_rank: usize,
    remaining: usize,
    stack: &mut Vec<usize>,
    out: &mut Vec<Vec<usize>>,
) {
    if remaining == 0 {
        out.push(stack.clone());
        return;
    }
    for rank in (0..=max_rank).rev() {
        let run = stack.iter().rev().take_while(|&&r| r == rank).count();
        if run >= 4 {
            continue;
        }
        stack.push(rank);
        build_rank_tuples(rank, remaining - 1, stack, out);
        stack.pop();
    }
}

/// k-подмножества мастей как битовые маски.
fn suit_subsets(k: usize) -> Vec<u8> {
    (0u8..16)
        .filter(|mask| mask.count_ones() as usize == k)
        .collect()
}

/// Все канонические классы бордов улицы в детерминированном порядке:
/// ранговые кортежи по убыванию, внутри кортежа — по каноническому
/// ключу. Кратность — число перечисленных бордов с этим ключом
/// (размер орбиты).
pub fn street_classes(street: Street) -> Vec<StreetClass> {
    let n = street.card_count();
    let mut classes: Vec<StreetClass> = Vec::new();
    for ranks in rank_tuples_desc(n) {
        // группы одинаковых рангов: (ранг, кратность)
        let mut groups: Vec<(usize, usize)> = Vec::new();
        for &rank in &ranks {
            match groups.last_mut() {
                Some(last) if last.0 == rank => last.1 += 1,
                _ => groups.push((rank, 1)),
            }
        }
        let subsets: Vec<Vec<u8>> = groups.iter().map(|&(_, k)| suit_subsets(k)).collect();
        let total: u64 = subsets.iter().map(|set| set.len() as u64).product();
        let mut orbits: Vec<([u8; 5], u64)> = Vec::new();
        let mut position: HashMap<[u8; 5], usize> = HashMap::new();
        let mut choice = vec![0usize; groups.len()];
        let mut enumerated = 0u64;
        loop {
            let mut board = [0xFFu8; 5];
            let mut slot = 0usize;
            for (group_index, &(rank, _)) in groups.iter().enumerate() {
                let mask = subsets[group_index][choice[group_index]];
                for suit in 0..4 {
                    if mask & (1 << suit) != 0 {
                        board[slot] = (rank * 4 + suit) as u8;
                        slot += 1;
                    }
                }
            }
            assert_eq!(slot, n);
            let key = canonical_key(&board[..n]);
            let next = orbits.len();
            let known = *position.entry(key).or_insert(next);
            if known == next {
                orbits.push((key, 1));
            } else {
                orbits[known].1 += 1;
            }
            enumerated += 1;
            // «одометр»: инкремент выбора подмножеств по группам
            let mut carry = true;
            let mut index = groups.len();
            while carry && index > 0 {
                index -= 1;
                choice[index] += 1;
                if choice[index] < subsets[index].len() {
                    carry = false;
                } else {
                    choice[index] = 0;
                }
            }
            if carry {
                break; // все комбинации перебраны
            }
        }
        assert_eq!(enumerated, total);
        let orbit_total: u64 = orbits.iter().map(|&(_, mult)| mult).sum();
        assert_eq!(orbit_total, total);
        orbits.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (key, mult) in orbits {
            classes.push(StreetClass {
                index: 0,
                street,
                rep: key,
                multiplicity: mult,
            });
        }
    }
    for (index, class) in classes.iter_mut().enumerate() {
        class.index = index;
    }
    classes
}

/// Индекс семейства ранговых мультиплетов (кратности по убыванию).
/// Тёрн: [4]=0, [3,1]=1, [2,2]=2, [2,1,1]=3, [1,1,1,1]=4.
/// Ривер: [4,1]=0, [3,2]=1, [3,1,1]=2, [2,2,1]=3, [2,1,1,1]=4, [1x5]=5.
/// ([5] невозможен: у ранга всего 4 масти.)
fn rank_family(ranks: &[usize]) -> usize {
    let mut mults: Vec<usize> = Vec::new();
    let mut run = 0usize;
    for (i, &rank) in ranks.iter().enumerate() {
        if i > 0 && rank == ranks[i - 1] {
            run += 1;
        } else {
            if run > 0 {
                mults.push(run);
            }
            run = 1;
        }
    }
    mults.push(run);
    mults.sort_unstable_by(|a, b| b.cmp(a));
    match mults.as_slice() {
        [4] => 0,
        [3, 1] => 1,
        [2, 2] => 2,
        [2, 1, 1] => 3,
        [1, 1, 1, 1] => 4,
        [4, 1] => 0,
        [3, 2] => 1,
        [3, 1, 1] => 2,
        [2, 2, 1] => 3,
        [2, 1, 1, 1] => 4,
        [1, 1, 1, 1, 1] => 5,
        other => unreachable!("мультиплеты {other:?} невозможны для 4/5 карт"),
    }
}

/// Форма мастей: (число мастей, максимум карт одной масти).
/// Тёрн: (4,1)=0, (3,2)=1, (2,2)=2, (2,3)=3, (1,4)=4.
/// Ривер: (5,1)=0, (4,2)=1, (3,2)=2, (3,3)=3, (2,3)=4, (2,4)=5, (1,5)=6.
/// Грубее позиционных паттернов флопа (hm/hl/ml): какая именно пара
/// позиций делит масть, различается эквити-слоем v2, а не v1.
fn flush_shape(street: Street, cards: &[u8]) -> usize {
    let mut counts = [0usize; 4];
    for &card in cards {
        counts[(card & 3) as usize] += 1;
    }
    let distinct = counts.iter().filter(|&&count| count > 0).count();
    let max = counts.into_iter().max().unwrap();
    match (street, distinct, max) {
        (Street::Turn, 4, 1) => 0,
        (Street::Turn, 3, 2) => 1,
        (Street::Turn, 2, 2) => 2,
        (Street::Turn, 2, 3) => 3,
        (Street::Turn, 1, 4) => 4,
        (Street::River, 5, 1) => 0,
        (Street::River, 4, 2) => 1,
        (Street::River, 3, 2) => 2,
        (Street::River, 3, 3) => 3,
        (Street::River, 2, 3) => 4,
        (Street::River, 2, 4) => 5,
        (Street::River, 1, 5) => 6,
        _ => unreachable!("форма мастей {distinct}/{max} невозможна для {street}"),
    }
}

/// Разброс рангов — как span_group флопа (пороги те же), для n карт.
fn span_group_n(ranks: &[usize], enabled: bool) -> usize {
    if !enabled {
        return 0;
    }
    let span = ranks[0] - ranks[ranks.len() - 1];
    if span <= 4 {
        0
    } else if span <= 8 {
        1
    } else {
        2
    }
}

fn street_fingerprint(street: Street, granularity: Granularity, buckets: &[usize]) -> u64 {
    let mut input: Vec<u8> = Vec::with_capacity(16 + buckets.len() * 8);
    input.push(street.tag());
    input.push(granularity_tag(granularity));
    input.extend_from_slice(&(buckets.len() as u64).to_le_bytes());
    for &bucket in buckets {
        input.extend_from_slice(&(bucket as u64).to_le_bytes());
    }
    fnv1a64(&input)
}

/// Структурная абстракция улицы (v1): детерминированные бакеты поверх
/// канонических классов, без эквити. Бакет = семейство рангов × форма
/// мастей × top/mid/span-группы Granularity — зеркало флопа v1.
pub struct StreetAbstraction {
    street: Street,
    granularity: Granularity,
    classes: Vec<StreetClass>,
    index_by_key: HashMap<[u8; 5], usize>,
    buckets: Vec<usize>,
    used: usize,
    fingerprint: u64,
}

impl StreetAbstraction {
    /// Полная генерация классов улицы (тёрн — ~секунды, ривер — секунды
    /// в release; в debug ривер дорог — полная проверка в #[ignore]-тесте).
    pub fn new(street: Street, granularity: Granularity) -> Self {
        let classes = street_classes(street);
        Self::from_classes(street, granularity, classes)
    }

    pub(crate) fn from_classes(
        street: Street,
        granularity: Granularity,
        classes: Vec<StreetClass>,
    ) -> Self {
        let (top_dim, mid_dim, span_dim): (usize, usize, usize) = match granularity {
            Granularity::Coarse => (2, 2, 1),
            Granularity::Medium => (4, 3, 1),
            Granularity::Fine => (4, 3, 3),
        };
        // flush_dim: тёрн 5 форм мастей; ривер 7. Family — ведущий индекс
        // смешанного кодирования бакета, его размерность в формуле не участвует.
        let flush_dim = match street {
            Street::Turn => 5,
            Street::River => 7,
        };
        let n = street.card_count();
        let mut index_by_key: HashMap<[u8; 5], usize> = HashMap::with_capacity(classes.len());
        let mut buckets = Vec::with_capacity(classes.len());
        for (index, class) in classes.iter().enumerate() {
            index_by_key.insert(class.rep, index);
            let ranks = class.ranks();
            let family = rank_family(&ranks);
            let flush = flush_shape(street, &class.rep[..n]);
            let top = top_group(ranks[0], granularity);
            let mid = mid_group(ranks[1], granularity);
            let span = span_group_n(&ranks, span_dim > 1);
            buckets.push(
                (((family * flush_dim + flush) * top_dim + top) * mid_dim + mid) * span_dim + span,
            );
        }
        let max_bucket = buckets.iter().copied().max().unwrap_or(0);
        let mut seen = vec![false; max_bucket + 1];
        for &bucket in &buckets {
            seen[bucket] = true;
        }
        let used = seen.iter().filter(|&&x| x).count();
        let fingerprint = street_fingerprint(street, granularity, &buckets);
        StreetAbstraction {
            street,
            granularity,
            classes,
            index_by_key,
            buckets,
            used,
            fingerprint,
        }
    }

    pub fn street(&self) -> Street {
        self.street
    }

    pub fn granularity(&self) -> Granularity {
        self.granularity
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    pub fn used_buckets(&self) -> usize {
        self.used
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn bucket_of(&self, class_index: usize) -> Option<usize> {
        self.buckets.get(class_index).copied()
    }

    pub fn classes(&self) -> &[StreetClass] {
        &self.classes
    }

    /// Индекс класса по конкретным картам борда (порядок любой);
    /// None при неверном числе карт, дубликатах, карте вне 0..52.
    pub fn class_index_of_cards(&self, cards: &[u8]) -> Option<usize> {
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
        self.index_by_key.get(&canonical_key(cards)).copied()
    }

    pub fn bucket_of_cards(&self, cards: &[u8]) -> Option<usize> {
        let class_index = self.class_index_of_cards(cards)?;
        self.buckets.get(class_index).copied()
    }

    /// Гистограмма по конкретным бордам: (бакет, Σ кратностей классов).
    pub fn histogram(&self) -> Vec<(usize, u64)> {
        let mut counts: HashMap<usize, u64> = HashMap::new();
        for (class, &bucket) in self.classes.iter().zip(self.buckets.iter()) {
            *counts.entry(bucket).or_insert(0) += class.multiplicity;
        }
        let mut out: Vec<(usize, u64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// Σ кратностей всех классов = C(52, n) — инвариант генерации.
    pub fn total_boards(&self) -> u64 {
        self.classes.iter().map(|class| class.multiplicity).sum()
    }
}

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

    #[test]
    fn canonical_key_monotone_suit_isomorphism() {
        // Роял одной масти: любая масть даёт один ключ (σ переводит).
        let spades = [
            c('A', 's'),
            c('K', 's'),
            c('Q', 's'),
            c('J', 's'),
            c('T', 's'),
        ];
        let hearts = [
            c('A', 'h'),
            c('K', 'h'),
            c('Q', 'h'),
            c('J', 'h'),
            c('T', 'h'),
        ];
        assert_eq!(canonical_key(&spades), [48, 44, 40, 36, 32]);
        assert_eq!(canonical_key(&hearts), canonical_key(&spades));
    }

    #[test]
    fn canonical_key_quads_self_canonical() {
        // Квады: все 4 масти — орбита из одного борда, ключ инвариантен.
        let quads = [c('A', 's'), c('A', 'h'), c('A', 'd'), c('A', 'c')];
        assert_eq!(canonical_key(&quads), [51, 50, 49, 48, 0xFF]);
    }

    #[test]
    fn canonical_key_position_partition_matters() {
        // «верх+средний одной масти» и «верх+нижний одной масти» —
        // разные классы (аналог two-tone-hm/two-tone-hl флопа).
        let high_mid = [c('A', 's'), c('K', 's'), c('Q', 'd'), c('J', 'c')];
        let high_low = [c('A', 's'), c('K', 'd'), c('Q', 's'), c('J', 'c')];
        assert_ne!(canonical_key(&high_mid), canonical_key(&high_low));
        // порядок аргумента не влияет
        let mut shuffled = high_mid;
        shuffled.reverse();
        assert_eq!(canonical_key(&shuffled), canonical_key(&high_mid));
    }

    #[test]
    fn rank_and_flush_families_unit() {
        assert_eq!(rank_family(&[12, 12, 12, 12]), 0);
        assert_eq!(rank_family(&[12, 12, 12, 4]), 1);
        assert_eq!(rank_family(&[12, 12, 11, 11]), 2);
        assert_eq!(rank_family(&[12, 12, 11, 5]), 3);
        assert_eq!(rank_family(&[11, 7, 5, 3]), 4);
        assert_eq!(rank_family(&[12, 12, 12, 12, 5]), 0);
        assert_eq!(rank_family(&[12, 12, 12, 5, 5]), 1);
        assert_eq!(rank_family(&[12, 12, 12, 5, 4]), 2);
        assert_eq!(rank_family(&[12, 12, 11, 11, 5]), 3);
        assert_eq!(rank_family(&[12, 12, 11, 5, 4]), 4);
        assert_eq!(rank_family(&[12, 11, 7, 5, 3]), 5);

        assert_eq!(
            flush_shape(
                Street::Turn,
                &[c('A', 's'), c('K', 's'), c('Q', 'd'), c('J', 'c')]
            ),
            1
        );
        assert_eq!(
            flush_shape(
                Street::Turn,
                &[c('A', 's'), c('K', 's'), c('Q', 's'), c('J', 'c')]
            ),
            3
        );
        assert_eq!(
            flush_shape(
                Street::River,
                &[
                    c('A', 's'),
                    c('K', 's'),
                    c('Q', 's'),
                    c('J', 's'),
                    c('T', 'c')
                ]
            ),
            5
        );
    }

    #[test]
    fn turn_classes_derived_by_burnside() {
        // Число классов — вывод леммой Бернсайда (действие S4 на
        // назначения мастей ранговых групп), не память:
        // [1,1,1,1]: C(13,4)=715 кортежей × 15 орбит (Bell(4);
        //   fix(id)=4^4, fix(transp)=2^4 ×6, fix(3-cycle)=1^4 ×8;
        //   (256+96+8)/24=15) = 10 725;
        // [2,1,1]: 13×C(12,2)=858 × 6 ((96+6×8)/24) = 5 148;
        // [2,2]: C(13,2)=78 × 3 ((36+6×4+3×4)/24) = 234;
        // [3,1]: 13×12=156 × 2 ((16+6×4+8×1)/24) = 312;
        // [4]: 13 × 1 = 13.
        // Итого 10 725 + 5 148 + 234 + 312 + 13 = 16 432.
        let classes = street_classes(Street::Turn);
        assert_eq!(classes.len(), 16_432);

        // Σ кратностей = C(52,4) = 270 725 (52·51·50·49/24).
        let total: u64 = classes.iter().map(|class| class.multiplicity).sum();
        assert_eq!(total, 270_725);

        // Квады: 13 классов, кратность 1 (единственный борд на ранг).
        let quads: Vec<&StreetClass> = classes
            .iter()
            .filter(|class| rank_family(&class.ranks()) == 0)
            .collect();
        assert_eq!(quads.len(), 13);
        assert!(quads.iter().all(|class| class.multiplicity == 1));

        // Монотонные с разными рангами: C(13,4)=715 классов,
        // кратность 4 (выбор единственной масти).
        let monotone: Vec<&StreetClass> = classes
            .iter()
            .filter(|class| flush_shape(Street::Turn, &class.rep[..4]) == 4)
            .collect();
        assert_eq!(monotone.len(), 715);
        assert!(monotone.iter().all(|class| class.multiplicity == 4));

        // Сквозные индексы.
        for (index, class) in classes.iter().enumerate() {
            assert_eq!(class.index, index);
        }
    }

    #[test]
    fn turn_brute_force_cross_check() {
        // Независимая генерация: все C(52,4) борда напрямую, без
        // ранговых кортежей. Гистограмма ключей обязана совпасть.
        let classes = street_classes(Street::Turn);
        let mut expected: HashMap<[u8; 5], u64> = HashMap::new();
        for a in 0..52u8 {
            for b in (a + 1)..52u8 {
                for c in (b + 1)..52u8 {
                    for d in (c + 1)..52u8 {
                        *expected.entry(canonical_key(&[a, b, c, d])).or_insert(0) += 1;
                    }
                }
            }
        }
        assert_eq!(expected.len(), classes.len());
        for class in &classes {
            assert_eq!(
                expected.get(&class.rep),
                Some(&class.multiplicity),
                "класс {} ({:?})",
                class.label(),
                class.rep
            );
        }
    }

    #[test]
    fn turn_abstraction_v1_properties() {
        let classes = street_classes(Street::Turn);
        let coarse =
            StreetAbstraction::from_classes(Street::Turn, Granularity::Coarse, classes.clone());
        let medium =
            StreetAbstraction::from_classes(Street::Turn, Granularity::Medium, classes.clone());
        let fine =
            StreetAbstraction::from_classes(Street::Turn, Granularity::Fine, classes.clone());
        let fine_again =
            StreetAbstraction::from_classes(Street::Turn, Granularity::Fine, classes.clone());

        // Детерминизм бакетирования (детерминизм генерации — конструктивно
        // + brute-force сверкой выше).
        assert_eq!(fine.fingerprint(), fine_again.fingerprint());
        assert_eq!(fine.used_buckets(), fine_again.used_buckets());
        assert_eq!(fine.histogram(), fine_again.histogram());

        // Вложенность разбиений: fine рефинирует medium, medium — coarse
        // (top/mid-группы Medium и Fine совпадают, Coarse — укрупнение).
        fn assert_refines(fine: &StreetAbstraction, coarse: &StreetAbstraction) {
            let mut mapping: HashMap<usize, usize> = HashMap::new();
            for index in 0..fine.class_count() {
                let fine_bucket = fine.bucket_of(index).unwrap();
                let coarse_bucket = coarse.bucket_of(index).unwrap();
                match mapping.get(&fine_bucket) {
                    Some(&existing) => assert_eq!(existing, coarse_bucket),
                    None => {
                        mapping.insert(fine_bucket, coarse_bucket);
                    }
                }
            }
        }
        assert_refines(&fine, &medium);
        assert_refines(&medium, &coarse);
        assert!(coarse.used_buckets() <= medium.used_buckets());
        assert!(medium.used_buckets() <= fine.used_buckets());

        // Гистограмма взвешена кратностями; инвариант тотала.
        assert_eq!(fine.class_count(), 16_432);
        assert_eq!(fine.total_boards(), 270_725);
        assert_eq!(
            fine.histogram()
                .iter()
                .map(|&(_, boards)| boards)
                .sum::<u64>(),
            270_725
        );

        // bucket_of_cards: канонизация в обе стороны, порядок карт не важен.
        let quads_index = classes
            .iter()
            .position(|class| class.rep == [51, 50, 49, 48, 0xFF])
            .unwrap();
        assert_eq!(
            fine.bucket_of_cards(&[c('A', 's'), c('A', 'h'), c('A', 'd'), c('A', 'c')]),
            fine.bucket_of(quads_index)
        );
        assert_eq!(
            fine.bucket_of_cards(&[c('A', 'c'), c('A', 'd'), c('A', 'h'), c('A', 's')]),
            fine.bucket_of(quads_index)
        );

        // Некорректные входы отклоняются.
        assert!(fine
            .bucket_of_cards(&[c('A', 's'), c('A', 'h'), c('A', 'd')])
            .is_none());
        assert!(fine
            .bucket_of_cards(&[c('A', 's'), c('A', 's'), c('A', 'd'), c('A', 'c')])
            .is_none());
        assert!(fine.bucket_of_cards(&[52, 0, 1, 2]).is_none());

        // Fingerprint различает гранулярности.
        assert_ne!(coarse.fingerprint(), medium.fingerprint());
        assert_ne!(medium.fingerprint(), fine.fingerprint());
    }

    #[test]
    #[ignore] // полная генерация ривера + brute-force: release, -- --ignored --nocapture
    fn streets_release_reference() {
        // Тёрн: числа для AI_LOG (корректность — debug-тестами выше).
        for granularity in [Granularity::Coarse, Granularity::Medium, Granularity::Fine] {
            let started = std::time::Instant::now();
            let abstraction = StreetAbstraction::new(Street::Turn, granularity);
            println!(
                "turn {granularity}: classes={} used={} fingerprint={:#018x} total_boards={} за {:?}",
                abstraction.class_count(),
                abstraction.used_buckets(),
                abstraction.fingerprint(),
                abstraction.total_boards(),
                started.elapsed()
            );
        }

        // Ривер, Бернсайд-вывод числа классов:
        // [1x5]: C(13,5)=1287 × 51 (Bell(5)-1; (4^5+6·2^5+8·1^5)/24) = 65 637;
        // [2,1,1,1]: 13×C(12,3)=2860 × 20 ((384+6×16)/24) = 57 200;
        // [2,2,1]: C(13,2)×11=858 × 8 ((144+6×8)/24) = 6 864;
        // [3,1,1]: 13×C(12,2)=858 × 5 ((64+6×8+8×1)/24) = 4 290;
        // [3,2]: 13×12=156 × 2 ((24+6×4)/24) = 312;
        // [4,1]: 13×12=156 × 1 ((4+6×2+8×1)/24) = 156.
        // Итого 134 459. Σ кратностей = C(52,5) = 2 598 960.
        let started = std::time::Instant::now();
        let classes = street_classes(Street::River);
        println!(
            "river: генерация {} классов за {:?}",
            classes.len(),
            started.elapsed()
        );
        assert_eq!(classes.len(), 134_459);
        assert_eq!(
            classes.iter().map(|class| class.multiplicity).sum::<u64>(),
            2_598_960
        );

        // Независимая brute-force сверка ривера — арбитр вывода.
        let mut expected: HashMap<[u8; 5], u64> = HashMap::new();
        for a in 0..52u8 {
            for b in (a + 1)..52u8 {
                for c in (b + 1)..52u8 {
                    for d in (c + 1)..52u8 {
                        for e in (d + 1)..52u8 {
                            *expected.entry(canonical_key(&[a, b, c, d, e])).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(expected.len(), classes.len());
        for class in &classes {
            assert_eq!(
                expected.get(&class.rep),
                Some(&class.multiplicity),
                "класс {}",
                class.label()
            );
        }
        println!(
            "river: brute-force сверка пройдена за {:?}",
            started.elapsed()
        );

        for granularity in [Granularity::Coarse, Granularity::Medium, Granularity::Fine] {
            let started = std::time::Instant::now();
            let abstraction = StreetAbstraction::new(Street::River, granularity);
            println!(
                "river {granularity}: classes={} used={} fingerprint={:#018x} total_boards={} за {:?}",
                abstraction.class_count(),
                abstraction.used_buckets(),
                abstraction.fingerprint(),
                abstraction.total_boards(),
                started.elapsed()
            );
        }
    }
}
