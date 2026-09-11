//! Базовые типы и операции с картами Texas Hold'em.
//!
//! Карта кодируется числом 0..51:
//! - rank = card >> 2: 0 = 2, ..., 12 = A;
//! - suit = card & 3: 0 = s, 1 = h, 2 = d, 3 = c.

use std::fmt;

pub type Card = u8;
pub type DeckMask = u64;

pub const DECK_SIZE: usize = 52;
pub const RANK_COUNT: usize = 13;
pub const SUIT_COUNT: usize = 4;
pub const RANKS: &[u8; 13] = b"23456789TJQKA";
pub const SUITS: [u8; 4] = [b's', b'h', b'd', b'c'];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CardParseError;

impl fmt::Display for CardParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid card")
    }
}

impl std::error::Error for CardParseError {}

#[inline]
pub const fn is_valid_card(card: Card) -> bool {
    card < DECK_SIZE as Card
}

#[inline]
pub const fn rank(card: Card) -> usize {
    (card >> 2) as usize
}

#[inline]
pub const fn suit(card: Card) -> usize {
    (card & 3) as usize
}

#[inline]
pub const fn card_mask(card: Card) -> DeckMask {
    1u64 << card
}

#[inline]
pub const fn full_deck_mask() -> DeckMask {
    (1u64 << 52) - 1
}

pub fn card_code(rank_char: char, suit_char: char) -> Option<Card> {
    let rank_index = RANKS.iter().position(|&c| c as char == rank_char)?;
    let suit_index = match suit_char {
        's' => 0,
        'h' => 1,
        'd' => 2,
        'c' => 3,
        _ => return None,
    };
    Some((rank_index * 4 + suit_index) as Card)
}

pub fn parse_card(token: &str) -> Result<Card, CardParseError> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() != 2 {
        return Err(CardParseError);
    }
    card_code(chars[0], chars[1]).ok_or(CardParseError)
}

pub fn card_str(card: Card) -> String {
    assert!(is_valid_card(card), "card code out of range: {card}");
    let mut result = String::with_capacity(2);
    result.push(RANKS[rank(card)] as char);
    result.push(SUITS[suit(card)] as char);
    result
}

pub fn cards_from_str(input: &str) -> Result<Vec<Card>, CardParseError> {
    input.split_whitespace().map(parse_card).collect()
}

pub fn mask_from_cards(cards: &[Card]) -> Result<DeckMask, CardParseError> {
    let mut mask = 0u64;
    for &card in cards {
        if !is_valid_card(card) {
            return Err(CardParseError);
        }
        let bit = card_mask(card);
        if mask & bit != 0 {
            return Err(CardParseError);
        }
        mask |= bit;
    }
    Ok(mask)
}

pub fn cards_from_mask(mask: DeckMask) -> Vec<Card> {
    (0..DECK_SIZE as Card)
        .filter(|&card| mask & card_mask(card) != 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_roundtrip() {
        for card in 0..52u8 {
            let text = card_str(card);
            assert_eq!(parse_card(&text).unwrap(), card);
        }
    }

    #[test]
    fn card_codes_match_expected_layout() {
        assert_eq!(card_code('A', 's'), Some(48));
        assert_eq!(card_code('A', 'c'), Some(51));
        assert_eq!(card_code('2', 's'), Some(0));
        assert_eq!(card_code('2', 'c'), Some(3));
    }

    #[test]
    fn duplicate_cards_are_rejected_by_mask() {
        let as_card = card_code('A', 's').unwrap();
        assert!(mask_from_cards(&[as_card, as_card]).is_err());
    }

    #[test]
    fn full_deck_has_52_cards() {
        assert_eq!(cards_from_mask(full_deck_mask()).len(), 52);
    }
}
