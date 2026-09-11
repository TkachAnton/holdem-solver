//! Blocker-aware public board sampling.

use holdem_cards::{card_mask, Card, DeckMask};
use holdem_ranges::Combo;

#[derive(Debug, Clone, PartialEq)]
pub struct BoardSample {
    pub board: Vec<Card>,
    pub probability: f64,
}

#[derive(Debug, Clone)]
pub struct BoardSampler {
    state: u64,
}

impl BoardSampler {
    pub fn new(seed: u64) -> Self {
        // Zero is a valid user seed, but a non-zero state avoids a degenerate
        // stream for generators that use zero as an absorbing state.
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    fn sample_index(&mut self, len: usize) -> usize {
        (self.next_u64() as usize) % len
    }
}

/// Samples public board completions while excluding every known hole card.
///
/// `known_hands` must contain all dealt hands, including folded players. The
/// returned samples are independent Monte-Carlo samples and may repeat the
/// same board; every sample has equal nominal probability.
pub fn sample_public_boards(
    known_hands: &[Combo],
    board: &[Card],
    new_card_count: usize,
    sample_count: usize,
    seed: u64,
) -> Result<Vec<BoardSample>, String> {
    if sample_count == 0 {
        return Err("sample_count must be positive".to_string());
    }
    if new_card_count == 0 || board.len() + new_card_count > 5 {
        return Err("invalid number of public cards to sample".to_string());
    }

    let mut dead: DeckMask = 0;
    for hand in known_hands {
        add_dead(&mut dead, hand.cards[0])?;
        add_dead(&mut dead, hand.cards[1])?;
    }
    for &card in board {
        add_dead(&mut dead, card)?;
    }

    let available: Vec<Card> = (0..52u8)
        .filter(|&card| dead & card_mask(card) == 0)
        .collect();
    if available.len() < new_card_count {
        return Err("not enough cards for public board sampling".to_string());
    }

    let probability = 1.0 / sample_count as f64;
    let mut sampler = BoardSampler::new(seed);
    let mut samples = Vec::with_capacity(sample_count);

    for _ in 0..sample_count {
        let mut remaining = available.clone();
        let mut completion = Vec::with_capacity(new_card_count);
        for _ in 0..new_card_count {
            let index = sampler.sample_index(remaining.len());
            completion.push(remaining.swap_remove(index));
        }
        completion.sort_unstable();

        let mut sampled_board = board.to_vec();
        sampled_board.extend_from_slice(&completion);
        samples.push(BoardSample {
            board: sampled_board,
            probability,
        });
    }

    Ok(samples)
}

fn add_dead(dead: &mut DeckMask, card: Card) -> Result<(), String> {
    if card >= 52 {
        return Err(format!("card out of range: {card}"));
    }
    let bit = card_mask(card);
    if *dead & bit != 0 {
        return Err(format!("duplicate known card: {card}"));
    }
    *dead |= bit;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;

    fn combo(text: &str) -> Combo {
        let cards = cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    #[test]
    fn samples_never_use_known_hole_cards_or_board_cards() {
        let hands = vec![combo("As Ah"), combo("Kc Kh"), combo("Qd Jd")];
        let board = cards_from_str("2s 7d 9c").unwrap();
        let samples = sample_public_boards(&hands, &board, 2, 100, 42).unwrap();

        assert_eq!(samples.len(), 100);
        for sample in samples {
            assert_eq!(sample.board.len(), 5);
            assert!(!sample
                .board
                .iter()
                .any(|card| { hands.iter().any(|hand| hand.cards.contains(card)) }));
            let mut unique = sample.board.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), 5);
            assert!((sample.probability - 0.01).abs() < 1e-12);
        }
    }

    #[test]
    fn sampler_is_reproducible_for_same_seed() {
        let hands = vec![combo("As Ah"), combo("Kc Kh")];
        let board = cards_from_str("2s 7d 9c").unwrap();
        let first = sample_public_boards(&hands, &board, 2, 20, 99).unwrap();
        let second = sample_public_boards(&hands, &board, 2, 20, 99).unwrap();
        assert_eq!(first, second);
    }
}
