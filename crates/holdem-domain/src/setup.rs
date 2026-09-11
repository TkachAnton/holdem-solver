//! Construction of initial tournament states.

use crate::table::{AnteMode, TableConfig};
use crate::{GameState, PlayerState, PlayerStatus, Street};

fn pay_forced_contribution(
    player: &mut PlayerState,
    amount: crate::Chips,
    counts_as_bet: bool,
) -> Result<(), String> {
    if amount < 0 {
        return Err("forced contribution cannot be negative".to_string());
    }
    if amount > player.stack_remaining {
        return Err(format!(
            "stack of seat {} is too short for forced contribution",
            player.seat
        ));
    }

    player.stack_remaining -= amount;
    player.committed_total += amount;
    if counts_as_bet {
        player.committed_street += amount;
    }
    Ok(())
}

/// Builds the initial preflop state from table configuration.
///
/// Antes are included in `committed_total` and therefore in the pot, but are
/// not included in `committed_street`: they are dead money and must not reduce
/// the amount required to call the big blind.
pub fn build_preflop_state(config: &TableConfig) -> Result<GameState, String> {
    config.validate()?;

    let mut players = Vec::with_capacity(config.table_size);
    for seat in 0..config.table_size {
        let mut player = PlayerState::new(seat, config.stacks[seat])?;
        player.position = config.position_for_seat(seat)?;

        let ante = match config.ante_mode {
            AnteMode::None => 0,
            AnteMode::Uniform => config.ante,
            AnteMode::BigBlind if seat == config.big_blind_seat() => config.ante,
            AnteMode::BigBlind => 0,
        };
        if ante > 0 {
            pay_forced_contribution(&mut player, ante, false)?;
            player.ante_paid = ante;
        }
        players.push(player);
    }

    let small_blind = config.small_blind_seat();
    let big_blind = config.big_blind_seat();
    pay_forced_contribution(&mut players[small_blind], config.small_blind, true)?;
    pay_forced_contribution(&mut players[big_blind], config.big_blind, true)?;

    if players[small_blind].stack_remaining == 0 {
        players[small_blind].status = PlayerStatus::AllIn;
    }
    if players[big_blind].stack_remaining == 0 {
        players[big_blind].status = PlayerStatus::AllIn;
    }

    let mut state = GameState::new(
        config.table_size,
        Street::Preflop,
        Vec::new(),
        players,
        config.dead_money,
    )?;

    let pending = config.preflop_order();
    let actor = pending
        .iter()
        .copied()
        .find(|&seat| state.players[seat].status.can_act())
        .ok_or_else(|| "no player can act preflop".to_string())?;

    state.configure_betting(
        actor,
        config.big_blind,
        config.big_blind,
        pending
            .into_iter()
            .filter(|&seat| state.players[seat].status.can_act())
            .collect(),
    )?;

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{AnteMode, Position};

    fn table(ante_mode: AnteMode) -> TableConfig {
        TableConfig {
            table_size: 8,
            button: 0,
            small_blind: 500,
            big_blind: 1_000,
            ante: 100,
            ante_mode,
            stacks: vec![100_000; 8],
            dead_money: 0,
        }
    }

    #[test]
    fn preflop_state_assigns_blinds_and_order() {
        let state = build_preflop_state(&table(AnteMode::None)).unwrap();
        assert_eq!(state.pot, 1_500);
        assert_eq!(state.current_bet, 1_000);
        assert_eq!(state.actor, Some(3));
        assert_eq!(state.pending_players, vec![3, 4, 5, 6, 7, 0, 1, 2]);
        assert_eq!(state.players[0].position, Position::Button);
        assert_eq!(state.players[2].position, Position::BigBlind);
        assert!(state.pot_invariant_holds());
    }

    #[test]
    fn uniform_ante_is_dead_money_not_call_contribution() {
        let state = build_preflop_state(&table(AnteMode::Uniform)).unwrap();
        assert_eq!(state.pot, 2_300);
        assert_eq!(state.players[0].ante_paid, 100);
        assert_eq!(state.players[3].committed_street, 0);
        assert_eq!(state.players[2].committed_street, 1_000);
        assert_eq!(state.current_bet, 1_000);
    }

    #[test]
    fn big_blind_ante_is_paid_by_big_blind_only() {
        let state = build_preflop_state(&table(AnteMode::BigBlind)).unwrap();
        assert_eq!(state.pot, 1_600);
        assert_eq!(state.players[2].ante_paid, 100);
        assert_eq!(state.players[0].ante_paid, 0);
        assert_eq!(state.players[2].committed_street, 1_000);
    }
}
