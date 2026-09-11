//! Main-pot and side-pot construction.
//!
//! Side pots are derived from total committed chips, not from current street
//! commitments. Folded players contribute money but are never eligible to win.

use crate::{Chips, PlayerId, PlayerState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PotLayer {
    pub index: usize,
    pub cap: Chips,
    pub amount: Chips,
    pub contributors: Vec<PlayerId>,
    pub eligible_players: Vec<PlayerId>,
}

impl PotLayer {
    pub fn is_claimable(&self) -> bool {
        !self.eligible_players.is_empty()
    }
}

/// Builds layered pots from all players' total commitments.
///
/// `dead_money` is added to the lowest layer because it is available to every
/// player who remains eligible for the main pot. A layer may have no eligible
/// players when it represents an uncalled excess; settlement must handle that
/// case explicitly rather than silently losing chips.
pub fn build_side_pots(
    players: &[PlayerState],
    dead_money: Chips,
) -> Result<Vec<PotLayer>, String> {
    if dead_money < 0 {
        return Err("dead money cannot be negative".to_string());
    }

    for player in players {
        if player.committed_total < 0 {
            return Err(format!("negative commitment for player {}", player.seat));
        }
    }

    let mut levels: Vec<Chips> = players
        .iter()
        .map(|player| player.committed_total)
        .filter(|&amount| amount > 0)
        .collect();
    levels.sort_unstable();
    levels.dedup();

    if levels.is_empty() {
        if dead_money == 0 {
            return Ok(Vec::new());
        }
        return Ok(vec![PotLayer {
            index: 0,
            cap: 0,
            amount: dead_money,
            contributors: Vec::new(),
            eligible_players: players
                .iter()
                .filter(|player| player.status.participates_in_showdown())
                .map(|player| player.seat)
                .collect(),
        }]);
    }

    let mut pots = Vec::with_capacity(levels.len());
    let mut previous = 0;

    for (index, cap) in levels.into_iter().enumerate() {
        let delta = cap
            .checked_sub(previous)
            .ok_or_else(|| "side-pot level order underflow".to_string())?;
        let contributors: Vec<PlayerId> = players
            .iter()
            .filter(|player| player.committed_total >= cap)
            .map(|player| player.seat)
            .collect();
        let contributor_count = Chips::try_from(contributors.len())
            .map_err(|_| "too many contributors for chip type".to_string())?;
        let mut amount = delta
            .checked_mul(contributor_count)
            .ok_or_else(|| "side-pot amount overflow".to_string())?;
        if index == 0 {
            amount = amount
                .checked_add(dead_money)
                .ok_or_else(|| "main pot amount overflow".to_string())?;
        }

        let eligible_players = contributors
            .iter()
            .copied()
            .filter(|seat| {
                players
                    .iter()
                    .find(|player| player.seat == *seat)
                    .map(|player| player.status.participates_in_showdown())
                    .unwrap_or(false)
            })
            .collect();

        pots.push(PotLayer {
            index,
            cap,
            amount,
            contributors,
            eligible_players,
        });
        previous = cap;
    }

    Ok(pots)
}

/// Splits every pot between its winners. Remainder chips are assigned using
/// `seat_order`, making odd-chip distribution deterministic.
pub fn distribute_pots(
    pots: &[PotLayer],
    winners_by_pot: &[Vec<PlayerId>],
    seat_order: &[PlayerId],
) -> Result<Vec<Chips>, String> {
    if pots.len() != winners_by_pot.len() {
        return Err("winner list must contain one entry per pot".to_string());
    }

    let max_seat = pots
        .iter()
        .flat_map(|pot| pot.contributors.iter().copied())
        .chain(seat_order.iter().copied())
        .max()
        .unwrap_or(0);
    let mut payouts = vec![0 as Chips; max_seat + 1];

    for (pot, winners) in pots.iter().zip(winners_by_pot) {
        if winners.is_empty() {
            return Err(format!("pot {} has no winners", pot.index));
        }
        let mut unique_winners = winners.clone();
        unique_winners.sort_unstable();
        unique_winners.dedup();
        for &winner in &unique_winners {
            if !pot.eligible_players.contains(&winner) {
                return Err(format!(
                    "player {winner} is not eligible for pot {}",
                    pot.index
                ));
            }
        }

        let winner_count = Chips::try_from(unique_winners.len())
            .map_err(|_| "too many winners for chip type".to_string())?;
        let share = pot.amount / winner_count;
        let remainder = (pot.amount % winner_count) as usize;

        for &winner in &unique_winners {
            payouts[winner] = payouts[winner]
                .checked_add(share)
                .ok_or_else(|| "payout overflow".to_string())?;
        }

        let ordered_winners: Vec<PlayerId> = seat_order
            .iter()
            .copied()
            .filter(|seat| unique_winners.contains(seat))
            .chain(
                unique_winners
                    .iter()
                    .copied()
                    .filter(|seat| !seat_order.contains(seat)),
            )
            .collect();
        for &winner in ordered_winners.iter().take(remainder) {
            payouts[winner] = payouts[winner]
                .checked_add(1)
                .ok_or_else(|| "odd-chip payout overflow".to_string())?;
        }
    }

    Ok(payouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlayerStatus;

    fn player(seat: usize, committed: Chips, status: PlayerStatus) -> PlayerState {
        let mut result = PlayerState::new(seat, 10_000).unwrap();
        result.committed_total = committed;
        result.committed_street = committed;
        result.status = status;
        result
    }

    #[test]
    fn builds_main_and_side_pots() {
        let players = vec![
            player(0, 100, PlayerStatus::AllIn),
            player(1, 100, PlayerStatus::Active),
            player(2, 300, PlayerStatus::Active),
        ];
        let pots = build_side_pots(&players, 0).unwrap();

        assert_eq!(pots.len(), 2);
        assert_eq!(pots[0].amount, 300);
        assert_eq!(pots[0].eligible_players, vec![0, 1, 2]);
        assert_eq!(pots[1].amount, 200);
        assert_eq!(pots[1].eligible_players, vec![2]);
    }

    #[test]
    fn folded_contributors_are_not_eligible() {
        let players = vec![
            player(0, 100, PlayerStatus::AllIn),
            player(1, 300, PlayerStatus::Active),
            player(2, 300, PlayerStatus::Folded),
        ];
        let pots = build_side_pots(&players, 0).unwrap();

        assert_eq!(pots[0].eligible_players, vec![0, 1]);
        assert_eq!(pots[1].contributors, vec![1, 2]);
        assert_eq!(pots[1].eligible_players, vec![1]);
    }

    #[test]
    fn dead_money_is_added_to_main_pot() {
        let players = vec![player(0, 100, PlayerStatus::Active)];
        let pots = build_side_pots(&players, 50).unwrap();
        assert_eq!(pots[0].amount, 150);
        assert_eq!(pots[0].eligible_players, vec![0]);
    }

    #[test]
    fn odd_chip_distribution_is_deterministic() {
        let players = vec![
            player(0, 100, PlayerStatus::Active),
            player(1, 100, PlayerStatus::Active),
        ];
        let pots = build_side_pots(&players, 1).unwrap();
        let payouts = distribute_pots(&pots, &[vec![0, 1]], &[1, 0]).unwrap();
        assert_eq!(payouts, vec![100, 101]);
    }
}
