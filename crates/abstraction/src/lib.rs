//! Абстракция флопа: 1755 канонических классов и feature-кластеризация.
//!
//! v1 (T3.1, D-016): детерминированные feature-бакеты. Никакого Монте-Карло:
//! чистая функция (класс флопа, Granularity) -> бакет, мгновенный fingerprint.
//! Эквити-кластеризация (OCHS-класс) — продолжение T3.1 поверх этого каркаса.
//!
//! Кодирование карт совпадает с holdem-cards: rank = card >> 2 (0='2'..12='A'),
//! suit = card & 3.

use std::collections::HashMap;
use std::fmt;

pub mod equity;

pub use equity::FlopEquityAbstraction;

pub mod street;

pub use street::{Street, StreetAbstraction, StreetClass};

pub mod street_equity;

pub use street_equity::StreetEquityAbstraction;

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

/// Канонический масти-паттерн флопа (масти-изоморфизм).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuitPattern {
    /// Три одинаковых ранга.
    Trips,
    /// Пара + кикер, три разные масти.
    PairedRainbow,
    /// Пара + кикер, делящий масть с одной из пары.
    PairedTwoTone,
    /// Три разных ранга, три разные масти.
    Rainbow,
    /// Старшая и средняя одной масти.
    TwoToneHighMid,
    /// Старшая и младшая одной масти.
    TwoToneHighLow,
    /// Средняя и младшая одной масти.
    TwoToneMidLow,
    /// Три одной масти.
    Monotone,
}

impl SuitPattern {
    /// 0 — радуга, 1 — две масти, 2 — монотон.
    fn flushiness(self) -> usize {
        match self {
            SuitPattern::Trips | SuitPattern::PairedRainbow | SuitPattern::Rainbow => 0,
            SuitPattern::PairedTwoTone
            | SuitPattern::TwoToneHighMid
            | SuitPattern::TwoToneHighLow
            | SuitPattern::TwoToneMidLow => 1,
            SuitPattern::Monotone => 2,
        }
    }
}

/// Канонический класс флопа.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlopClass {
    pub index: usize,
    /// Ранги по убыванию.
    pub ranks: [usize; 3],
    pub pattern: SuitPattern,
    /// Представитель класса: три конкретные карты.
    pub rep: [u8; 3],
}

impl FlopClass {
    pub fn label(&self) -> String {
        let r: String = self.ranks.iter().map(|&x| rank_char(x)).collect();
        let p = match self.pattern {
            SuitPattern::Trips => "trips",
            SuitPattern::PairedRainbow => "paired-rainbow",
            SuitPattern::PairedTwoTone => "paired-two-tone",
            SuitPattern::Rainbow => "rainbow",
            SuitPattern::TwoToneHighMid => "two-tone-hm",
            SuitPattern::TwoToneHighLow => "two-tone-hl",
            SuitPattern::TwoToneMidLow => "two-tone-ml",
            SuitPattern::Monotone => "monotone",
        };
        format!("{r} {p}")
    }
}

/// Все 1755 канонических классов флопов, детерминированный порядок.
pub fn flop_classes() -> Vec<FlopClass> {
    let mut out: Vec<FlopClass> = Vec::with_capacity(1755);
    // Трипсы {a,a,a}: 13 классов.
    for a in 0..13usize {
        out.push(FlopClass {
            index: 0,
            ranks: [a, a, a],
            pattern: SuitPattern::Trips,
            rep: [mk_card(a, 0), mk_card(a, 1), mk_card(a, 2)],
        });
    }
    // Пары {a,a,b}: 13*12 ранговых паттернов x 2 класса = 312.
    for p in 0..13usize {
        for k in 0..13usize {
            if k == p {
                continue;
            }
            let ranks = if p > k { [p, p, k] } else { [k, p, p] };
            out.push(FlopClass {
                index: 0,
                ranks,
                pattern: SuitPattern::PairedRainbow,
                rep: [mk_card(p, 0), mk_card(p, 1), mk_card(k, 2)],
            });
            out.push(FlopClass {
                index: 0,
                ranks,
                pattern: SuitPattern::PairedTwoTone,
                rep: [mk_card(p, 0), mk_card(p, 1), mk_card(k, 0)],
            });
        }
    }
    // Различные ранги h > m > l: C(13,3) = 286 x 5 классов = 1430.
    for h in (0..13usize).rev() {
        for m in (0..h).rev() {
            for l in (0..m).rev() {
                out.push(FlopClass {
                    index: 0,
                    ranks: [h, m, l],
                    pattern: SuitPattern::Rainbow,
                    rep: [mk_card(h, 0), mk_card(m, 1), mk_card(l, 2)],
                });
                out.push(FlopClass {
                    index: 0,
                    ranks: [h, m, l],
                    pattern: SuitPattern::TwoToneHighMid,
                    rep: [mk_card(h, 0), mk_card(m, 0), mk_card(l, 2)],
                });
                out.push(FlopClass {
                    index: 0,
                    ranks: [h, m, l],
                    pattern: SuitPattern::TwoToneHighLow,
                    rep: [mk_card(h, 0), mk_card(m, 1), mk_card(l, 0)],
                });
                out.push(FlopClass {
                    index: 0,
                    ranks: [h, m, l],
                    pattern: SuitPattern::TwoToneMidLow,
                    rep: [mk_card(h, 0), mk_card(m, 1), mk_card(l, 1)],
                });
                out.push(FlopClass {
                    index: 0,
                    ranks: [h, m, l],
                    pattern: SuitPattern::Monotone,
                    rep: [mk_card(h, 0), mk_card(m, 0), mk_card(l, 0)],
                });
            }
        }
    }
    for (index, class) in out.iter_mut().enumerate() {
        class.index = index;
    }
    out
}

/// Канонический класс трёх конкретных карт флопа (порядок любой).
pub fn flop_class_of_cards(cards: &[u8]) -> Option<FlopClass> {
    if cards.len() != 3 {
        return None;
    }
    let mut seen = [false; 52];
    for &card in cards {
        if card >= 52 || seen[card as usize] {
            return None;
        }
        seen[card as usize] = true;
    }
    let mut ordered: Vec<u8> = cards.to_vec();
    ordered.sort_by_key(|&card| (std::cmp::Reverse(rank_of(card)), suit_of(card)));
    let r: [usize; 3] = [
        rank_of(ordered[0]),
        rank_of(ordered[1]),
        rank_of(ordered[2]),
    ];
    let s: [usize; 3] = [
        suit_of(ordered[0]),
        suit_of(ordered[1]),
        suit_of(ordered[2]),
    ];
    let pattern = if r[0] == r[1] && r[1] == r[2] {
        SuitPattern::Trips
    } else if r[0] == r[1] || r[1] == r[2] {
        // Пара: r[1]==r[2] — пара младшая (кикер старший), иначе пара старшая.
        let pair_is_low = r[1] == r[2];
        let kicker_suit = if pair_is_low { s[0] } else { s[2] };
        let pair_suits = if pair_is_low {
            [s[1], s[2]]
        } else {
            [s[0], s[1]]
        };
        if kicker_suit == pair_suits[0] || kicker_suit == pair_suits[1] {
            SuitPattern::PairedTwoTone
        } else {
            SuitPattern::PairedRainbow
        }
    } else {
        let distinct = s[0] != s[1] && s[1] != s[2] && s[0] != s[2];
        if distinct {
            SuitPattern::Rainbow
        } else if s[0] == s[1] && s[1] == s[2] {
            SuitPattern::Monotone
        } else if s[0] == s[1] {
            SuitPattern::TwoToneHighMid
        } else if s[0] == s[2] {
            SuitPattern::TwoToneHighLow
        } else {
            SuitPattern::TwoToneMidLow
        }
    };
    flop_classes()
        .into_iter()
        .find(|c| c.ranks == r && c.pattern == pattern)
}

/// Грубость feature-кластеризации: размер бакетов задаётся конфигом.
/// Разбиения вложены: бакет Coarse — объединение бакетов Medium; Fine
/// дополнительно режет по связности. Поэтому used монотонен по гранулярности.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Granularity {
    Coarse,
    Medium,
    Fine,
}

impl fmt::Display for Granularity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Granularity::Coarse => write!(f, "coarse"),
            Granularity::Medium => write!(f, "medium"),
            Granularity::Fine => write!(f, "fine"),
        }
    }
}

fn top_group(rank: usize, granularity: Granularity) -> usize {
    match granularity {
        Granularity::Coarse => {
            if rank >= 8 {
                0
            } else {
                1
            }
        }
        _ => {
            if rank >= 11 {
                0
            } else if rank >= 8 {
                1
            } else if rank >= 5 {
                2
            } else {
                3
            }
        }
    }
}

fn mid_group(rank: usize, granularity: Granularity) -> usize {
    match granularity {
        Granularity::Coarse => {
            if rank >= 5 {
                0
            } else {
                1
            }
        }
        _ => {
            if rank >= 9 {
                0
            } else if rank >= 5 {
                1
            } else {
                2
            }
        }
    }
}

fn span_group(ranks: [usize; 3], enabled: bool) -> usize {
    if !enabled {
        return 0;
    }
    let span = ranks[0] - ranks[2];
    if span <= 4 {
        0
    } else if span <= 8 {
        1
    } else {
        2
    }
}

fn granularity_tag(granularity: Granularity) -> u8 {
    match granularity {
        Granularity::Coarse => 1,
        Granularity::Medium => 2,
        Granularity::Fine => 3,
    }
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Feature-абстракция флопов: детерминированная, без случайности.
/// HashMap используется только для гистограммы и полностью сортируется
/// на выходе — порядок итерации RandomState не влияет ни на что видимое.
pub struct FlopAbstraction {
    granularity: Granularity,
    classes: Vec<FlopClass>,
    buckets: Vec<usize>,
    used: usize,
    fingerprint: u64,
}

impl FlopAbstraction {
    pub fn new(granularity: Granularity) -> Self {
        let classes = flop_classes();
        let (top_dim, mid_dim, span_dim): (usize, usize, usize) = match granularity {
            Granularity::Coarse => (2, 2, 1),
            Granularity::Medium => (4, 3, 1),
            Granularity::Fine => (4, 3, 3),
        };
        let buckets: Vec<usize> = classes
            .iter()
            .map(|class| {
                let paired = match class.pattern {
                    SuitPattern::Trips => 2,
                    SuitPattern::PairedRainbow | SuitPattern::PairedTwoTone => 1,
                    _ => 0,
                };
                let flush = class.pattern.flushiness();
                let top = top_group(class.ranks[0], granularity);
                let mid = mid_group(class.ranks[1], granularity);
                let span = span_group(class.ranks, span_dim > 1);
                (((paired * 3 + flush) * top_dim + top) * mid_dim + mid) * span_dim + span
            })
            .collect();
        let slots = top_dim * mid_dim * span_dim * 9;
        let mut used_set = vec![false; slots];
        for &b in &buckets {
            used_set[b] = true;
        }
        let used = used_set.iter().filter(|&&x| x).count();
        let mut hash_input = Vec::with_capacity(1 + buckets.len() * 8);
        hash_input.push(granularity_tag(granularity));
        for &b in &buckets {
            hash_input.extend_from_slice(&(b as u64).to_le_bytes());
        }
        let fingerprint = fnv1a64(&hash_input);
        FlopAbstraction {
            granularity,
            classes,
            buckets,
            used,
            fingerprint,
        }
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

    pub fn bucket_of_cards(&self, cards: &[u8]) -> Option<usize> {
        let class = flop_class_of_cards(cards)?;
        self.buckets.get(class.index).copied()
    }

    pub fn classes(&self) -> &[FlopClass] {
        &self.classes
    }

    /// Гистограмма: (бакет, число флопов), по убыванию числа, затем по id.
    pub fn histogram(&self) -> Vec<(usize, usize)> {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for &b in &self.buckets {
            *counts.entry(b).or_insert(0) += 1;
        }
        let mut out: Vec<(usize, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
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
            _ => panic!("неизвестная масть"),
        };
        mk_card(r, s)
    }

    #[test]
    fn class_count_is_derived() {
        // Выводим, а не вспоминаем: трипсы 13; пары 13*12 ранговых
        // паттернов x 2 масти-класса; различные ранги C(13,3) x 5.
        let expected = 13 + 13 * 12 * 2 + 286 * 5;
        assert_eq!(expected, 1755);
        assert_eq!(flop_classes().len(), expected);
    }

    #[test]
    fn representatives_are_valid_cards() {
        for class in flop_classes() {
            let mut seen = [false; 52];
            for &card in &class.rep {
                assert!(card < 52, "карта вне диапазона в {}", class.label());
                assert!(!seen[card as usize], "дубликат карты в {}", class.label());
                seen[card as usize] = true;
            }
        }
    }

    #[test]
    fn representatives_roundtrip_to_their_class_and_bucket() {
        let fine = FlopAbstraction::new(Granularity::Fine);
        for class in flop_classes() {
            let found = flop_class_of_cards(&class.rep)
                .unwrap_or_else(|| panic!("представитель не найден: {}", class.label()));
            assert_eq!(found, class);
            let mut shuffled = class.rep.clone();
            shuffled.reverse();
            assert_eq!(flop_class_of_cards(&shuffled), Some(class.clone()));
            assert_eq!(
                fine.bucket_of_cards(&class.rep),
                fine.bucket_of(class.index),
                "бакет представителя не совпал: {}",
                class.label()
            );
        }
    }

    #[test]
    fn mapping_is_deterministic() {
        for g in [Granularity::Coarse, Granularity::Medium, Granularity::Fine] {
            let a = FlopAbstraction::new(g);
            let b = FlopAbstraction::new(g);
            assert_eq!(a.fingerprint(), b.fingerprint());
            assert_eq!(a.used_buckets(), b.used_buckets());
            assert_eq!(a.histogram(), b.histogram());
        }
    }

    #[test]
    fn granularity_ladder_is_nested() {
        let coarse = FlopAbstraction::new(Granularity::Coarse);
        let medium = FlopAbstraction::new(Granularity::Medium);
        let fine = FlopAbstraction::new(Granularity::Fine);
        assert!(
            coarse.used_buckets() <= medium.used_buckets(),
            "coarse {} > medium {}",
            coarse.used_buckets(),
            medium.used_buckets()
        );
        assert!(
            medium.used_buckets() < fine.used_buckets(),
            "medium {} >= fine {}",
            medium.used_buckets(),
            fine.used_buckets()
        );
        assert!(
            fine.used_buckets() >= 100,
            "Fine подозрительно мал: {}",
            fine.used_buckets()
        );
        let fps = [
            coarse.fingerprint(),
            medium.fingerprint(),
            fine.fingerprint(),
        ];
        assert_ne!(fps[0], fps[1]);
        assert_ne!(fps[1], fps[2]);
        assert_ne!(fps[0], fps[2]);
    }

    #[test]
    fn suit_and_pair_sensitivity() {
        let fine = FlopAbstraction::new(Granularity::Fine);
        let mono = fine
            .bucket_of_cards(&[c('A', 'h'), c('K', 'h'), c('Q', 'h')])
            .unwrap();
        let two_tone = fine
            .bucket_of_cards(&[c('A', 'h'), c('K', 'h'), c('Q', 'd')])
            .unwrap();
        let rainbow = fine
            .bucket_of_cards(&[c('A', 'h'), c('K', 'd'), c('Q', 'c')])
            .unwrap();
        assert_ne!(mono, two_tone, "монотон и две масти неразличимы");
        assert_ne!(two_tone, rainbow, "две масти и радуга неразличимы");
        let pair_tt = fine
            .bucket_of_cards(&[c('K', 's'), c('K', 'h'), c('9', 's')])
            .unwrap();
        let pair_rb = fine
            .bucket_of_cards(&[c('K', 's'), c('K', 'h'), c('9', 'd')])
            .unwrap();
        assert_ne!(pair_tt, pair_rb, "парный two-tone и радуга неразличимы");
    }

    #[test]
    fn invalid_cards_rejected() {
        assert!(flop_class_of_cards(&[c('A', 'h'), c('A', 'h'), c('K', 'd')]).is_none());
        assert!(flop_class_of_cards(&[c('A', 'h'), c('K', 'd')]).is_none());
    }
}
