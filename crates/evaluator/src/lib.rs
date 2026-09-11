//! Единый evaluator для 5–7 карт Texas Hold'em.
//!
//! Внешний API всегда валидирует duplicate cards и поддерживает 5, 6 и 7 карт.
//! Внутри все руки сравниваются одним лексикографически закодированным значением.

use holdem_cards::{card_mask, rank, suit, Card};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum HandCategory {
    HighCard = 0,
    OnePair = 1,
    TwoPair = 2,
    ThreeOfAKind = 3,
    Straight = 4,
    Flush = 5,
    FullHouse = 6,
    FourOfAKind = 7,
    StraightFlush = 8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct HandValue(pub u32);

impl HandValue {
    pub fn category(self) -> HandCategory {
        match ((self.0 >> 20) & 0xF) as u8 {
            0 => HandCategory::HighCard,
            1 => HandCategory::OnePair,
            2 => HandCategory::TwoPair,
            3 => HandCategory::ThreeOfAKind,
            4 => HandCategory::Straight,
            5 => HandCategory::Flush,
            6 => HandCategory::FullHouse,
            7 => HandCategory::FourOfAKind,
            8 => HandCategory::StraightFlush,
            _ => HandCategory::HighCard,
        }
    }
}

fn encode(category: HandCategory, values: &[u8]) -> HandValue {
    let mut value = (category as u32) << 20;
    for (index, &part) in values.iter().take(5).enumerate() {
        value |= ((part as u32) & 0xF) << (16 - index * 4);
    }
    HandValue(value)
}

fn straight_high(rank_mask: u16) -> Option<u8> {
    for high in (4u8..=12u8).rev() {
        let mask = 0b1_1111u16 << (high - 4);
        if rank_mask & mask == mask {
            return Some(high);
        }
    }

    // A-2-3-4-5. Rank 12 is ace and rank 0 is deuce.
    let wheel = (1u16 << 12) | (1u16 << 3) | (1u16 << 2) | (1u16 << 1) | 1u16;
    if rank_mask & wheel == wheel {
        Some(3)
    } else {
        None
    }
}

/// Оценка ровно пяти предварительно проверенных карт.
pub fn evaluate_five(cards: &[Card; 5]) -> HandValue {
    let mut rank_counts = [0u8; 13];
    let mut rank_mask = 0u16;
    let flush = cards.iter().all(|&card| suit(card) == suit(cards[0]));

    for &card in cards {
        let current_rank = rank(card);
        rank_counts[current_rank] += 1;
        rank_mask |= 1u16 << current_rank;
    }

    let straight = straight_high(rank_mask);
    if flush {
        if let Some(high) = straight {
            return encode(HandCategory::StraightFlush, &[high]);
        }

        let mut ranks = Vec::with_capacity(5);
        for current_rank in (0..13).rev() {
            if rank_counts[current_rank] > 0 {
                ranks.push(current_rank as u8);
            }
        }
        return encode(HandCategory::Flush, &ranks);
    }

    let mut quads = None;
    let mut trips = Vec::new();
    let mut pairs = Vec::new();

    for current_rank in (0..13).rev() {
        match rank_counts[current_rank] {
            4 => quads = Some(current_rank as u8),
            3 => trips.push(current_rank as u8),
            2 => pairs.push(current_rank as u8),
            _ => {}
        }
    }

    if let Some(quad_rank) = quads {
        let kicker = (0..13)
            .rev()
            .find(|&current_rank| current_rank as u8 != quad_rank && rank_counts[current_rank] > 0)
            .unwrap_or(0) as u8;
        return encode(HandCategory::FourOfAKind, &[quad_rank, kicker]);
    }

    if !trips.is_empty() && (!pairs.is_empty() || trips.len() >= 2) {
        let triple = trips[0];
        let pair = if trips.len() >= 2 { trips[1] } else { pairs[0] };
        return encode(HandCategory::FullHouse, &[triple, pair]);
    }

    if let Some(&triple) = trips.first() {
        let kickers: Vec<u8> = (0..13)
            .rev()
            .filter(|&current_rank| current_rank as u8 != triple && rank_counts[current_rank] > 0)
            .map(|current_rank| current_rank as u8)
            .take(2)
            .collect();
        return encode(
            HandCategory::ThreeOfAKind,
            &[triple, kickers[0], kickers[1]],
        );
    }

    if pairs.len() >= 2 {
        let kicker = (0..13)
            .rev()
            .find(|&current_rank| rank_counts[current_rank] == 1)
            .unwrap_or(0) as u8;
        return encode(HandCategory::TwoPair, &[pairs[0], pairs[1], kicker]);
    }

    if let Some(&pair) = pairs.first() {
        let kickers: Vec<u8> = (0..13)
            .rev()
            .filter(|&current_rank| current_rank as u8 != pair && rank_counts[current_rank] > 0)
            .map(|current_rank| current_rank as u8)
            .take(3)
            .collect();
        return encode(
            HandCategory::OnePair,
            &[pair, kickers[0], kickers[1], kickers[2]],
        );
    }

    let high_cards: Vec<u8> = (0..13)
        .rev()
        .filter(|&current_rank| rank_counts[current_rank] > 0)
        .map(|current_rank| current_rank as u8)
        .collect();
    if let Some(high) = straight {
        return encode(HandCategory::Straight, &[high]);
    }
    encode(HandCategory::HighCard, &high_cards)
}

fn enumerate_five(
    cards: &[Card],
    start: usize,
    depth: usize,
    selected: &mut [Card; 5],
    best: &mut HandValue,
) {
    if depth == 5 {
        let current = evaluate_five(selected);
        if current > *best {
            *best = current;
        }
        return;
    }

    let remaining_slots = 5 - depth;
    if cards.len() - start < remaining_slots {
        return;
    }

    for index in start..cards.len() {
        selected[depth] = cards[index];
        enumerate_five(cards, index + 1, depth + 1, selected, best);
    }
}

/// Оценка руки из 5, 6 или 7 карт.
pub fn evaluate(cards: &[Card]) -> Result<HandValue, String> {
    if !(5..=7).contains(&cards.len()) {
        return Err(format!("expected 5..=7 cards, got {}", cards.len()));
    }

    let mut mask = 0u64;
    for &card in cards {
        if card >= 52 {
            return Err(format!("card out of range: {card}"));
        }
        let bit = card_mask(card);
        if mask & bit != 0 {
            return Err("duplicate card in evaluator input".to_string());
        }
        mask |= bit;
    }

    let mut selected = [0u8; 5];
    let mut best = HandValue::default();
    enumerate_five(cards, 0, 0, &mut selected, &mut best);
    Ok(best)
}

pub fn evaluate7(cards: &[Card; 7]) -> Result<HandValue, String> {
    evaluate(cards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;

    fn evaluate_text(text: &str) -> HandValue {
        let cards = cards_from_str(text).unwrap();
        evaluate(&cards).unwrap()
    }

    #[test]
    fn categories_are_ordered() {
        assert!(HandCategory::Straight > HandCategory::ThreeOfAKind);
        assert!(HandCategory::StraightFlush > HandCategory::FourOfAKind);
    }

    #[test]
    fn detects_royal_flush() {
        let value = evaluate_text("As Ks Qs Js Ts 3d 4c");
        assert_eq!(value.category(), HandCategory::StraightFlush);
    }

    #[test]
    fn detects_wheel_on_five_cards() {
        let value = evaluate_text("As 2h 3d 4c 5s");
        assert_eq!(value.category(), HandCategory::Straight);
        assert_eq!(value.0 & 0xFFFFF, 3 << 16);
    }

    #[test]
    fn supports_six_cards() {
        let value = evaluate_text("As Ah Kd Qc Js 2h");
        assert_eq!(value.category(), HandCategory::OnePair);
    }

    #[test]
    fn full_house_beats_flush() {
        let full_house = evaluate_text("2c 7c 2s 2h 7s 7h Ks");
        let flush = evaluate_text("As Qs Ts 2s 3s 7h Kd");
        assert!(full_house > flush);
    }

    #[test]
    fn duplicate_cards_are_rejected() {
        let cards = cards_from_str("As As Kd Qc Js").unwrap();
        assert!(evaluate(&cards).is_err());
    }
}
