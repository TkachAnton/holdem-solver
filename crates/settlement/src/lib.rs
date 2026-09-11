//! Terminal ChipEV settlement over main and side pots.
//!
//! This layer combines domain pot construction with exact profile equity. The
//! result is an expected payout/EV vector, which is the representation needed
//! by CFR at a chance-aware showdown terminal.

use holdem_domain::pot::{build_side_pots, PotLayer};
use holdem_domain::GameState;
use holdem_equity::exact_profile_equity;
use holdem_ranges::Combo;

#[derive(Debug, Clone, PartialEq)]
pub struct PotSettlement {
    pub pot: PotLayer,
    pub shares: Vec<f64>,
    pub expected_payouts: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShowdownSettlement {
    pub pots: Vec<PotSettlement>,
    pub expected_payouts: Vec<f64>,
    pub net_ev: Vec<f64>,
}

/// Computes exact expected ChipEV payouts for every pot.
///
/// `hands` must contain every dealt hand, including folded players. The equity
/// engine uses all of them as dead cards but evaluates only the eligible
/// players of each individual pot.
pub fn exact_chip_ev_showdown(
    state: &GameState,
    hands: &[Combo],
) -> Result<ShowdownSettlement, String> {
    state.validate()?;
    if hands.len() != state.players.len() {
        return Err(format!(
            "expected {} dealt hands, got {}",
            state.players.len(),
            hands.len()
        ));
    }
    if state.board.len() < 3 {
        return Err("exact showdown settlement requires at least a flop".to_string());
    }
    if state.active_players().len() < 2 {
        return Err("showdown requires at least two active players".to_string());
    }

    let pots = build_side_pots(&state.players, state.dead_money)?;
    if pots.is_empty() {
        return Err("cannot settle an empty pot".to_string());
    }

    let mut expected_payouts = vec![0.0; state.table_size];
    let mut settlements = Vec::with_capacity(pots.len());

    for pot in pots {
        if pot.eligible_players.is_empty() {
            return Err(format!("pot {} has no eligible showdown player", pot.index));
        }

        let equity = exact_profile_equity(&hands, &pot.eligible_players, &state.board)?;
        let mut pot_expected = vec![0.0; state.table_size];
        for &player in &pot.eligible_players {
            let share = equity
                .shares
                .get(player)
                .copied()
                .ok_or_else(|| format!("equity missing player {player}"))?;
            pot_expected[player] = pot.amount as f64 * share;
            expected_payouts[player] += pot_expected[player];
        }

        settlements.push(PotSettlement {
            pot,
            shares: equity.shares,
            expected_payouts: pot_expected,
        });
    }

    let net_ev = expected_payouts
        .iter()
        .enumerate()
        .map(|(player, &payout)| payout - state.players[player].committed_total as f64)
        .collect();

    Ok(ShowdownSettlement {
        pots: settlements,
        expected_payouts,
        net_ev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;
    use holdem_domain::{GameState, PlayerState, PlayerStatus, Street};

    fn combo(text: &str) -> Combo {
        let cards = cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    #[test]
    fn exact_showdown_pays_main_pot_by_equity() {
        let mut players = vec![
            PlayerState::new(0, 900).unwrap(),
            PlayerState::new(1, 900).unwrap(),
        ];
        for player in &mut players {
            player.committed_total = 100;
            player.committed_street = 100;
        }
        let board = cards_from_str("2s 7d 9c").unwrap();
        let state = GameState::new(2, Street::Flop, board, players, 0).unwrap();
        let result = exact_chip_ev_showdown(&state, &[combo("As Ah"), combo("Kc Kh")]).unwrap();

        assert_eq!(result.pots.len(), 1);
        assert_eq!(result.pots[0].pot.amount, 200);
        assert_eq!(result.expected_payouts.len(), 2);
        assert!(result.expected_payouts[0] > result.expected_payouts[1]);
        let total: f64 = result.expected_payouts.iter().sum();
        assert!((total - 200.0).abs() < 1e-9);
    }

    #[test]
    fn side_pot_equity_excludes_folded_player_from_eligibility() {
        let mut players = vec![
            PlayerState::new(0, 900).unwrap(),
            PlayerState::new(1, 700).unwrap(),
            PlayerState::new(2, 700).unwrap(),
        ];
        players[0].committed_total = 100;
        players[0].committed_street = 100;
        players[1].committed_total = 300;
        players[1].committed_street = 300;
        players[2].committed_total = 300;
        players[2].committed_street = 300;
        players[2].status = PlayerStatus::Folded;

        let board = cards_from_str("2s 7d 9c").unwrap();
        let state = GameState::new(3, Street::Flop, board, players, 0).unwrap();
        let result =
            exact_chip_ev_showdown(&state, &[combo("As Ah"), combo("Kc Kh"), combo("Qd Jd")])
                .unwrap();

        assert_eq!(result.pots.len(), 2);
        assert_eq!(result.pots[0].pot.eligible_players, vec![0, 1]);
        assert_eq!(result.pots[1].pot.eligible_players, vec![1]);
        assert!((result.expected_payouts.iter().sum::<f64>() - 700.0).abs() < 1e-9);
    }
}
