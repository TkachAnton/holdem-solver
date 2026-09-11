//! Exact profile equity for known hole cards and a public board.
//!
//! The first foundation slice supports exact enumeration when at most two
//! board cards are unknown (flop/turn/river). All hole cards, including cards
//! of folded players, are dead cards for future runouts.

use holdem_cards::{card_mask, Card, DeckMask};
use holdem_evaluator::evaluate;
use holdem_ranges::Combo;

pub mod sampler;
pub use sampler::{sample_public_boards, BoardSample, BoardSampler};

#[derive(Debug, Clone, PartialEq)]
pub struct EquityResult {
    pub shares: Vec<f64>,
    pub runouts: u64,
}

fn add_card_to_mask(mask: &mut DeckMask, card: Card) -> Result<(), String> {
    if card >= 52 {
        return Err(format!("card out of range: {card}"));
    }
    let bit = card_mask(card);
    if *mask & bit != 0 {
        return Err(format!("duplicate card detected: {card}"));
    }
    *mask |= bit;
    Ok(())
}

fn score_runout(
    hands: &[Combo],
    active_players: &[usize],
    board: &[Card],
    shares: &mut [f64],
) -> Result<(), String> {
    let mut best = None;
    let mut winners = Vec::new();

    for &player in active_players {
        let hand = hands
            .get(player)
            .ok_or_else(|| format!("active player index out of range: {player}"))?;
        let mut seven = Vec::with_capacity(7);
        seven.push(hand.cards[0]);
        seven.push(hand.cards[1]);
        seven.extend_from_slice(board);
        if seven.len() != 7 {
            return Err("showdown board must contain exactly five cards".to_string());
        }

        let value = evaluate(&seven)?;
        match best {
            None => {
                best = Some(value);
                winners.clear();
                winners.push(player);
            }
            Some(current_best) if value > current_best => {
                best = Some(value);
                winners.clear();
                winners.push(player);
            }
            Some(current_best) if value == current_best => {
                winners.push(player);
            }
            Some(_) => {}
        }
    }

    if winners.is_empty() {
        return Err("no active players at showdown".to_string());
    }

    let share = 1.0 / winners.len() as f64;
    for player in winners {
        shares[player] += share;
    }
    Ok(())
}

fn validate_profile(
    hands: &[Combo],
    active_players: &[usize],
    board: &[Card],
) -> Result<DeckMask, String> {
    if hands.len() < 2 {
        return Err("profile must contain at least two players".to_string());
    }
    if board.len() > 5 {
        return Err("board cannot contain more than five cards".to_string());
    }
    if 5usize.saturating_sub(board.len()) > 2 {
        return Err("exact foundation equity supports at most two unknown board cards".to_string());
    }
    if active_players.is_empty() {
        return Err("at least one active player is required".to_string());
    }

    let mut active_seen = vec![false; hands.len()];
    for &player in active_players {
        if player >= hands.len() {
            return Err(format!("active player index out of range: {player}"));
        }
        if active_seen[player] {
            return Err(format!("active player listed twice: {player}"));
        }
        active_seen[player] = true;
    }

    let mut dead = 0u64;
    for hand in hands {
        add_card_to_mask(&mut dead, hand.cards[0])?;
        add_card_to_mask(&mut dead, hand.cards[1])?;
    }
    for &card in board {
        add_card_to_mask(&mut dead, card)?;
    }
    Ok(dead)
}

/// Returns exact showdown shares for a known profile.
///
/// The profile contains every dealt hand, not only active players. This is
/// intentional: folded players' cards must remain unavailable as future
/// board cards.
pub fn exact_profile_equity(
    hands: &[Combo],
    active_players: &[usize],
    board: &[Card],
) -> Result<EquityResult, String> {
    let dead = validate_profile(hands, active_players, board)?;
    let missing = 5 - board.len();
    let remaining: Vec<Card> = (0..52u8)
        .filter(|&card| dead & card_mask(card) == 0)
        .collect();

    let mut shares = vec![0.0; hands.len()];
    let mut runouts = 0u64;

    match missing {
        0 => {
            score_runout(hands, active_players, board, &mut shares)?;
            runouts = 1;
        }
        1 => {
            for &card in &remaining {
                let mut full_board = board.to_vec();
                full_board.push(card);
                score_runout(hands, active_players, &full_board, &mut shares)?;
                runouts += 1;
            }
        }
        2 => {
            for first_index in 0..remaining.len() {
                for second_index in (first_index + 1)..remaining.len() {
                    let mut full_board = board.to_vec();
                    full_board.push(remaining[first_index]);
                    full_board.push(remaining[second_index]);
                    score_runout(hands, active_players, &full_board, &mut shares)?;
                    runouts += 1;
                }
            }
        }
        _ => unreachable!("validate_profile rejects more than two unknown cards"),
    }

    if runouts == 0 {
        return Err("no legal board runouts".to_string());
    }

    for share in &mut shares {
        *share /= runouts as f64;
    }

    Ok(EquityResult { shares, runouts })
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
    fn exact_flop_equity_is_normalized() {
        let hands = vec![combo("As Ah"), combo("Kc Kh")];
        let board = cards_from_str("2s 7d 9c").unwrap();
        let result = exact_profile_equity(&hands, &[0, 1], &board).unwrap();

        // 52 - 4 hole cards - 3 board cards = 45 remaining cards.
        assert_eq!(result.runouts, 45 * 44 / 2);
        let total: f64 = result.shares.iter().sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert!(result.shares[0] > 0.5);
        assert!(result.shares[1] < 0.5);
    }

    #[test]
    fn folded_cards_are_dead_for_runouts() {
        let hands = vec![combo("As Ah"), combo("Kc Kh"), combo("Qd Jd")];
        let board = cards_from_str("2s 7d 9c").unwrap();
        let result = exact_profile_equity(&hands, &[0, 1], &board).unwrap();

        // 52 - 6 hole cards - 3 board cards = 43 remaining cards.
        assert_eq!(result.runouts, 43 * 42 / 2);
    }

    #[test]
    fn exact_foundation_rejects_preflop_runout_count() {
        let hands = vec![combo("As Ah"), combo("Kc Kh")];
        assert!(exact_profile_equity(&hands, &[0, 1], &[]).is_err());
    }
}
