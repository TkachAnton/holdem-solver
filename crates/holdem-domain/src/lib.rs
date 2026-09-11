//! Базовая доменная модель NLHE.
//!
//! Здесь пока нет полного tree builder и solver. Задача foundation slice —
//! зафиксировать корректное состояние денег, игроков, улицы и действий.

use holdem_cards::{mask_from_cards, Card};

pub type Chips = i64;
pub type PlayerId = usize;

pub mod pot;
pub mod setup;
pub mod table;

use table::Position;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Street {
    Preflop,
    Flop,
    Turn,
    River,
}

impl Street {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Preflop => Some(Self::Flop),
            Self::Flop => Some(Self::Turn),
            Self::Turn => Some(Self::River),
            Self::River => None,
        }
    }

    pub fn required_new_board_cards(self) -> usize {
        match self {
            Self::Preflop => 3,
            Self::Flop | Self::Turn => 1,
            Self::River => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayerStatus {
    Active,
    Folded,
    AllIn,
    OutOfHand,
}

impl PlayerStatus {
    pub fn can_act(self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn participates_in_showdown(self) -> bool {
        matches!(self, Self::Active | Self::AllIn)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerState {
    pub seat: usize,
    pub position: Position,
    pub stack_remaining: Chips,
    pub committed_total: Chips,
    pub committed_street: Chips,
    pub ante_paid: Chips,
    pub status: PlayerStatus,
}

impl PlayerState {
    pub fn new(seat: usize, stack_remaining: Chips) -> Result<Self, String> {
        if stack_remaining < 0 {
            return Err("stack cannot be negative".to_string());
        }
        Ok(Self {
            seat,
            position: Position::Unknown,
            stack_remaining,
            committed_total: 0,
            committed_street: 0,
            ante_paid: 0,
            status: PlayerStatus::Active,
        })
    }

    pub fn with_position(mut self, position: Position) -> Self {
        self.position = position;
        self
    }

    pub fn total_chips_in_hand(&self) -> Chips {
        self.stack_remaining + self.committed_total
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Fold,
    Check,
    Call,
    Bet { to: Chips },
    Raise { to: Chips },
    AllIn,
}

impl Action {
    pub fn target_amount(&self) -> Option<Chips> {
        match self {
            Self::Bet { to } | Self::Raise { to } => Some(*to),
            Self::Fold | Self::Check | Self::Call | Self::AllIn => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActionSizes {
    pub bet_to: Vec<Chips>,
    pub raise_to: Vec<Chips>,
    pub include_all_in: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRecord {
    pub player: PlayerId,
    pub street: Street,
    pub action: Action,
    pub pot_before: Chips,
    pub pot_after: Chips,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalState {
    Fold { winner: PlayerId },
    Showdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameState {
    pub table_size: usize,
    pub street: Street,
    pub board: Vec<Card>,
    pub players: Vec<PlayerState>,
    pub dead_money: Chips,
    pub pot: Chips,
    pub actor: Option<PlayerId>,
    pub current_bet: Chips,
    pub min_raise_increment: Chips,
    pub pending_players: Vec<PlayerId>,
    /// Players who are still allowed to make a raise in this betting round.
    /// A short all-in does not restore this right for players who already acted.
    pub raise_allowed: Vec<PlayerId>,
    pub action_history: Vec<ActionRecord>,
    pub terminal: Option<TerminalState>,
}

impl GameState {
    pub fn new(
        table_size: usize,
        street: Street,
        board: Vec<Card>,
        players: Vec<PlayerState>,
        dead_money: Chips,
    ) -> Result<Self, String> {
        let mut state = Self {
            table_size,
            street,
            board,
            players,
            dead_money,
            pot: 0,
            actor: None,
            current_bet: 0,
            min_raise_increment: 0,
            pending_players: Vec::new(),
            raise_allowed: Vec::new(),
            action_history: Vec::new(),
            terminal: None,
        };
        state.recompute_pot()?;
        state.validate()?;
        Ok(state)
    }

    /// Configures the public betting context before tree expansion or action replay.
    pub fn configure_betting(
        &mut self,
        actor: PlayerId,
        current_bet: Chips,
        min_raise_increment: Chips,
        pending_players: Vec<PlayerId>,
    ) -> Result<(), String> {
        if current_bet < 0 || min_raise_increment < 0 {
            return Err("bet and raise increment cannot be negative".to_string());
        }
        self.current_bet = current_bet;
        self.min_raise_increment = min_raise_increment;
        self.pending_players = pending_players;
        self.raise_allowed = self.pending_players.clone();
        self.actor = Some(actor);
        self.validate()
    }

    /// Returns legal actions for the current actor using the supplied action sizes.
    pub fn legal_actions(&self, sizes: &ActionSizes) -> Result<Vec<Action>, String> {
        let actor = self.actor.ok_or_else(|| "state has no actor".to_string())?;
        let player = self
            .players
            .iter()
            .find(|player| player.seat == actor)
            .ok_or_else(|| format!("actor seat does not exist: {actor}"))?;
        if !player.status.can_act() {
            return Err(format!("player cannot act: {actor}"));
        }

        let mut actions = vec![Action::Fold];
        let to_call = (self.current_bet - player.committed_street).max(0);
        let all_in_target = player.committed_street + player.stack_remaining;

        if to_call == 0 {
            actions.push(Action::Check);
        } else if to_call < player.stack_remaining {
            actions.push(Action::Call);
        }

        let can_raise = self.raise_allowed.contains(&actor);
        let maximum = all_in_target;
        if can_raise && self.current_bet == 0 {
            for &to in &sizes.bet_to {
                if to > 0 && to <= maximum {
                    actions.push(Action::Bet { to });
                }
            }
        } else if can_raise {
            for &to in &sizes.raise_to {
                let increment = to - self.current_bet;
                if to > self.current_bet && to <= maximum && increment >= self.min_raise_increment {
                    actions.push(Action::Raise { to });
                }
            }
        }

        let must_call_all_in = to_call >= player.stack_remaining && player.stack_remaining > 0;
        let all_in_is_raise = all_in_target > self.current_bet;
        let all_in_allowed = !all_in_is_raise || can_raise;
        if (sizes.include_all_in || must_call_all_in)
            && all_in_allowed
            && player.stack_remaining > 0
        {
            let duplicate_target = actions
                .iter()
                .any(|action| action.target_amount() == Some(all_in_target));
            if !duplicate_target {
                actions.push(Action::AllIn);
            }
        }

        Ok(actions)
    }

    /// Applies one action and updates pot, commitments, actor and pending players.
    /// Street transitions are intentionally left to the tree builder: when the
    /// pending list becomes empty, `actor` becomes `None` and the builder can
    /// add a chance node or advance to the next street.
    pub fn apply_action(&mut self, action: Action) -> Result<(), String> {
        let actor = self.actor.ok_or_else(|| "state has no actor".to_string())?;
        let player_index = self
            .players
            .iter()
            .position(|player| player.seat == actor)
            .ok_or_else(|| format!("actor seat does not exist: {actor}"))?;

        if !self.players[player_index].status.can_act() {
            return Err(format!("player cannot act: {actor}"));
        }
        if !self.pending_players.contains(&actor) {
            return Err(format!("actor is not pending: {actor}"));
        }

        let pot_before = self.pot;
        match action.clone() {
            Action::Fold => {
                self.players[player_index].status = PlayerStatus::Folded;
                self.remove_pending(actor);

                let active = self.active_players();
                if active.len() == 1 {
                    self.terminal = Some(TerminalState::Fold { winner: active[0] });
                    self.pending_players.clear();
                    self.actor = None;
                } else {
                    self.select_next_actor();
                }
            }
            Action::Check => {
                if self.players[player_index].committed_street != self.current_bet {
                    return Err("cannot check while facing a bet".to_string());
                }
                self.remove_pending(actor);
                self.select_next_actor();
            }
            Action::Call => {
                let to_call = self.current_bet - self.players[player_index].committed_street;
                if to_call <= 0 {
                    return Err("call is not legal when no chips are owed".to_string());
                }
                self.commit_to(actor, self.current_bet)?;
                if self.players[player_index].stack_remaining == 0 {
                    self.players[player_index].status = PlayerStatus::AllIn;
                }
                self.remove_pending(actor);
                self.select_next_actor();
            }
            Action::Bet { to } => {
                if !self.raise_allowed.contains(&actor) {
                    return Err("player is not allowed to bet in this round".to_string());
                }
                if self.current_bet != 0 {
                    return Err("bet is only legal when current bet is zero".to_string());
                }
                if to <= 0 {
                    return Err("bet target must be positive".to_string());
                }
                self.commit_to(actor, to)?;
                self.current_bet = to;
                self.min_raise_increment = to;
                self.reset_pending_after_aggression(actor, to);
                self.select_next_actor();
            }
            Action::Raise { to } => {
                if !self.raise_allowed.contains(&actor) {
                    return Err("player is not allowed to raise in this round".to_string());
                }
                if self.current_bet <= 0 || to <= self.current_bet {
                    return Err("raise target must exceed current bet".to_string());
                }
                let increment = to - self.current_bet;
                if increment < self.min_raise_increment {
                    return Err(format!(
                        "raise increment {increment} is below minimum {}",
                        self.min_raise_increment
                    ));
                }
                self.commit_to(actor, to)?;
                self.current_bet = to;
                self.min_raise_increment = increment;
                self.reset_pending_after_aggression(actor, to);
                self.select_next_actor();
            }
            Action::AllIn => {
                let target = self.players[player_index].committed_street
                    + self.players[player_index].stack_remaining;
                if target <= self.players[player_index].committed_street {
                    return Err("player has no chips left".to_string());
                }
                if target > self.current_bet && !self.raise_allowed.contains(&actor) {
                    return Err("player cannot reopen betting with all-in".to_string());
                }
                self.commit_to(actor, target)?;
                self.players[player_index].status = PlayerStatus::AllIn;

                if target > self.current_bet {
                    let increment = target - self.current_bet;
                    if increment >= self.min_raise_increment {
                        self.current_bet = target;
                        self.min_raise_increment = increment;
                        self.reset_pending_after_aggression(actor, target);
                    } else {
                        // Short all-in raise: players who are below the new
                        // target must respond, but the raise does not reopen
                        // raising for players who have already acted.
                        self.current_bet = target;
                        self.remove_pending(actor);
                        self.pending_players = self
                            .ordered_active_players_after(actor)
                            .into_iter()
                            .filter(|&player| {
                                self.players
                                    .iter()
                                    .find(|state| state.seat == player)
                                    .map(|state| state.committed_street < target)
                                    .unwrap_or(false)
                            })
                            .collect();
                    }
                } else {
                    self.remove_pending(actor);
                }
                self.select_next_actor();
            }
        }

        self.action_history.push(ActionRecord {
            player: actor,
            street: self.street,
            action,
            pot_before,
            pot_after: self.pot,
        });
        self.validate()
    }

    fn commit_to(&mut self, player: PlayerId, target: Chips) -> Result<(), String> {
        let index = self
            .players
            .iter()
            .position(|state| state.seat == player)
            .ok_or_else(|| format!("player seat does not exist: {player}"))?;
        let current = self.players[index].committed_street;
        if target < current {
            return Err("target commitment cannot decrease".to_string());
        }
        let amount = target - current;
        if amount > self.players[index].stack_remaining {
            return Err("player does not have enough chips".to_string());
        }

        self.players[index].stack_remaining -= amount;
        self.players[index].committed_street = target;
        self.players[index].committed_total += amount;
        self.pot += amount;
        Ok(())
    }

    fn remove_pending(&mut self, player: PlayerId) {
        self.pending_players.retain(|&pending| pending != player);
        self.raise_allowed.retain(|&allowed| allowed != player);
    }

    fn ordered_active_players_after(&self, player: PlayerId) -> Vec<PlayerId> {
        let mut result = Vec::new();
        for offset in 1..=self.table_size {
            let seat = (player + offset) % self.table_size;
            if self
                .players
                .iter()
                .find(|state| state.seat == seat)
                .map(|state| state.status.can_act())
                .unwrap_or(false)
            {
                result.push(seat);
            }
        }
        result
    }

    fn reset_pending_after_aggression(&mut self, actor: PlayerId, target: Chips) {
        self.pending_players = self
            .ordered_active_players_after(actor)
            .into_iter()
            .filter(|&player| {
                self.players
                    .iter()
                    .find(|state| state.seat == player)
                    .map(|state| state.committed_street < target)
                    .unwrap_or(false)
            })
            .collect();
        self.raise_allowed = self.pending_players.clone();
    }

    fn select_next_actor(&mut self) {
        self.actor = self.pending_players.first().copied();
    }

    /// Advances to the next street after a betting round has completed.
    ///
    /// The caller supplies the public chance cards for this transition. This
    /// keeps the state machine deterministic and lets the future tree builder
    /// represent the cards as explicit chance-node outcomes.
    pub fn advance_to_next_street(
        &mut self,
        new_cards: &[Card],
        action_order: &[PlayerId],
    ) -> Result<(), String> {
        if self.terminal.is_some() {
            return Err("cannot advance a terminal state".to_string());
        }
        if self.actor.is_some() || !self.pending_players.is_empty() {
            return Err("betting round is not complete".to_string());
        }

        let next_street = self
            .street
            .next()
            .ok_or_else(|| "river has no next street".to_string())?;
        let expected_existing = match self.street {
            Street::Preflop => 0,
            Street::Flop => 3,
            Street::Turn => 4,
            Street::River => 5,
        };
        if self.board.len() != expected_existing {
            return Err(format!(
                "board length {} does not match {:?}",
                self.board.len(),
                self.street
            ));
        }
        if new_cards.len() != self.street.required_new_board_cards() {
            return Err(format!(
                "expected {} new board cards on {:?}, got {}",
                self.street.required_new_board_cards(),
                self.street,
                new_cards.len()
            ));
        }

        let mut next_board = self.board.clone();
        next_board.extend_from_slice(new_cards);
        mask_from_cards(&next_board).map_err(|error| error.to_string())?;

        let active = self.active_players();
        if active.is_empty() {
            return Err("cannot advance without active showdown players".to_string());
        }
        if active.len() == 1 {
            self.board = next_board;
            self.street = next_street;
            self.terminal = Some(TerminalState::Fold { winner: active[0] });
            self.actor = None;
            self.pending_players.clear();
            self.raise_allowed.clear();
            return self.validate();
        }

        let mut seen_order = Vec::new();
        for &player in action_order {
            if player >= self.table_size {
                return Err(format!("action-order seat out of range: {player}"));
            }
            if seen_order.contains(&player) {
                return Err(format!("duplicate seat in action order: {player}"));
            }
            seen_order.push(player);
        }

        let acting = self.acting_players();
        for &player in &acting {
            if !action_order.contains(&player) {
                return Err(format!(
                    "acting player {player} is missing from action order"
                ));
            }
        }

        self.board = next_board;
        self.street = next_street;
        for player in &mut self.players {
            player.committed_street = 0;
        }
        self.current_bet = 0;
        self.min_raise_increment = 0;
        self.pending_players = action_order
            .iter()
            .copied()
            .filter(|player| acting.contains(player))
            .collect();
        self.raise_allowed = self.pending_players.clone();
        self.actor = self.pending_players.first().copied();
        self.validate()
    }

    pub fn recompute_pot(&mut self) -> Result<(), String> {
        if self.dead_money < 0 {
            return Err("dead money cannot be negative".to_string());
        }
        let committed: Chips = self
            .players
            .iter()
            .map(|player| player.committed_total)
            .sum();
        self.pot = self.dead_money + committed;
        Ok(())
    }

    pub fn pot_invariant_holds(&self) -> bool {
        let committed: Chips = self
            .players
            .iter()
            .map(|player| player.committed_total)
            .sum();
        self.pot == self.dead_money + committed
    }

    pub fn active_players(&self) -> Vec<PlayerId> {
        self.players
            .iter()
            .filter(|player| player.status.participates_in_showdown())
            .map(|player| player.seat)
            .collect()
    }

    pub fn acting_players(&self) -> Vec<PlayerId> {
        self.players
            .iter()
            .filter(|player| player.status.can_act())
            .map(|player| player.seat)
            .collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(2..=8).contains(&self.table_size) {
            return Err(format!("table size must be 2..=8, got {}", self.table_size));
        }
        if self.players.len() != self.table_size {
            return Err(format!(
                "table size {} does not match player count {}",
                self.table_size,
                self.players.len()
            ));
        }
        if self.board.len() > 5 {
            return Err("board cannot contain more than five cards".to_string());
        }
        mask_from_cards(&self.board).map_err(|error| error.to_string())?;

        let mut seen_seats = [false; 8];
        for player in &self.players {
            if player.seat >= self.table_size || player.seat >= seen_seats.len() {
                return Err(format!("invalid player seat: {}", player.seat));
            }
            if seen_seats[player.seat] {
                return Err(format!("duplicate player seat: {}", player.seat));
            }
            seen_seats[player.seat] = true;
            if player.stack_remaining < 0
                || player.committed_total < 0
                || player.committed_street < 0
                || player.ante_paid < 0
            {
                return Err(format!("negative chip value for player {}", player.seat));
            }
            if player.ante_paid > player.committed_total {
                return Err(format!(
                    "ante exceeds total commitment for player {}",
                    player.seat
                ));
            }
            if player.committed_street > player.committed_total {
                return Err(format!(
                    "street commitment exceeds total commitment for player {}",
                    player.seat
                ));
            }
        }

        if self.pot < 0 || !self.pot_invariant_holds() {
            return Err("pot invariant is violated".to_string());
        }

        if let Some(actor) = self.actor {
            let player = self
                .players
                .iter()
                .find(|player| player.seat == actor)
                .ok_or_else(|| format!("actor seat does not exist: {actor}"))?;
            if !player.status.can_act() {
                return Err(format!("actor cannot act: {actor:?}"));
            }
        }

        let mut seen_pending = [false; 8];
        for &player in &self.pending_players {
            if player >= seen_pending.len() || seen_pending[player] {
                return Err(format!("duplicate pending player: {player}"));
            }
            seen_pending[player] = true;
            let state = self
                .players
                .iter()
                .find(|state| state.seat == player)
                .ok_or_else(|| format!("pending player does not exist: {player}"))?;
            if !state.status.can_act() {
                return Err(format!("pending player cannot act: {player}"));
            }
        }

        let mut seen_raise = [false; 8];
        for &player in &self.raise_allowed {
            if player >= seen_raise.len() || seen_raise[player] {
                return Err(format!("duplicate raise-right player: {player}"));
            }
            seen_raise[player] = true;
            let state = self
                .players
                .iter()
                .find(|state| state.seat == player)
                .ok_or_else(|| format!("raise-right player does not exist: {player}"))?;
            if !state.status.can_act() {
                return Err(format!("raise-right player cannot act: {player}"));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;

    fn players() -> Vec<PlayerState> {
        vec![
            PlayerState::new(0, 100_000).unwrap(),
            PlayerState::new(1, 100_000).unwrap(),
            PlayerState::new(2, 100_000).unwrap(),
        ]
    }

    #[test]
    fn pot_is_initialized_from_committed_and_dead_money() {
        let mut players = players();
        players[0].committed_total = 500;
        players[0].committed_street = 500;
        let state = GameState::new(3, Street::Preflop, Vec::new(), players, 1_500).unwrap();
        assert_eq!(state.pot, 2_000);
        assert!(state.pot_invariant_holds());
    }

    #[test]
    fn board_duplicates_are_rejected() {
        let players = players();
        let board = cards_from_str("As As 2d").unwrap();
        assert!(GameState::new(3, Street::Flop, board, players, 0).is_err());
    }

    #[test]
    fn folded_player_is_not_active_or_able_to_act() {
        let mut players = players();
        players[1].status = PlayerStatus::Folded;
        let state = GameState::new(3, Street::Flop, Vec::new(), players, 0).unwrap();
        assert_eq!(state.active_players(), vec![0, 2]);
        assert_eq!(state.acting_players(), vec![0, 2]);
    }

    #[test]
    fn bet_updates_pot_and_resets_pending_players() {
        let mut state = GameState::new(3, Street::Flop, Vec::new(), players(), 0).unwrap();
        state.configure_betting(0, 0, 0, vec![0, 1, 2]).unwrap();
        state.apply_action(Action::Bet { to: 1_000 }).unwrap();

        assert_eq!(state.pot, 1_000);
        assert_eq!(state.current_bet, 1_000);
        assert_eq!(state.actor, Some(1));
        assert_eq!(state.pending_players, vec![1, 2]);
        assert_eq!(state.players[0].committed_street, 1_000);
        assert!(state.pot_invariant_holds());
    }

    #[test]
    fn all_remaining_players_folding_creates_fold_terminal() {
        let mut state = GameState::new(3, Street::Flop, Vec::new(), players(), 0).unwrap();
        state.configure_betting(0, 0, 0, vec![0, 1, 2]).unwrap();
        state.apply_action(Action::Bet { to: 1_000 }).unwrap();
        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();

        assert_eq!(state.terminal, Some(TerminalState::Fold { winner: 0 }));
        assert_eq!(state.actor, None);
        assert!(state.pot_invariant_holds());
    }

    #[test]
    fn raise_must_reach_minimum_increment() {
        let mut state = GameState::new(3, Street::Preflop, Vec::new(), players(), 0).unwrap();
        state
            .configure_betting(0, 1_000, 1_000, vec![0, 1, 2])
            .unwrap();
        assert!(state.apply_action(Action::Raise { to: 1_500 }).is_err());
        state.apply_action(Action::Raise { to: 2_000 }).unwrap();
        assert_eq!(state.current_bet, 2_000);
        assert_eq!(state.min_raise_increment, 1_000);
    }

    #[test]
    fn short_all_in_does_not_reopen_raise_for_previous_actor() {
        let mut committed_players = vec![
            PlayerState::new(0, 900).unwrap(),
            PlayerState::new(1, 50).unwrap(),
            PlayerState::new(2, 900).unwrap(),
        ];
        for player in &mut committed_players {
            player.committed_total = 100;
            player.committed_street = 100;
        }
        let mut state = GameState::new(3, Street::Flop, Vec::new(), committed_players, 0).unwrap();
        state.configure_betting(0, 100, 100, vec![0, 1, 2]).unwrap();

        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::AllIn).unwrap();

        assert_eq!(state.current_bet, 150);
        assert_eq!(state.pending_players, vec![2, 0]);
        assert_eq!(state.raise_allowed, vec![2]);

        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.actor, Some(0));
        let actions = state
            .legal_actions(&ActionSizes {
                bet_to: Vec::new(),
                raise_to: vec![300],
                include_all_in: true,
            })
            .unwrap();
        assert!(actions.contains(&Action::Call));
        assert!(!actions.contains(&Action::Raise { to: 300 }));
        assert!(!actions.contains(&Action::AllIn));
    }

    #[test]
    fn completed_preflop_round_can_advance_to_flop() {
        let mut state = GameState::new(
            2,
            Street::Preflop,
            Vec::new(),
            vec![
                PlayerState::new(0, 100_000).unwrap(),
                PlayerState::new(1, 100_000).unwrap(),
            ],
            0,
        )
        .unwrap();
        state.configure_betting(0, 0, 0, vec![0, 1]).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        assert_eq!(state.actor, None);

        let flop = cards_from_str("As 7d 2c").unwrap();
        state.advance_to_next_street(&flop, &[1, 0]).unwrap();

        assert_eq!(state.street, Street::Flop);
        assert_eq!(state.board, flop);
        assert_eq!(state.current_bet, 0);
        assert_eq!(state.pending_players, vec![1, 0]);
        assert_eq!(state.actor, Some(1));
        assert!(state
            .players
            .iter()
            .all(|player| player.committed_street == 0));
    }

    #[test]
    fn street_transition_requires_completed_betting_round() {
        let mut state = GameState::new(
            2,
            Street::Preflop,
            Vec::new(),
            vec![
                PlayerState::new(0, 100_000).unwrap(),
                PlayerState::new(1, 100_000).unwrap(),
            ],
            0,
        )
        .unwrap();
        state.configure_betting(0, 0, 0, vec![0, 1]).unwrap();
        let flop = cards_from_str("As 7d 2c").unwrap();
        assert!(state.advance_to_next_street(&flop, &[1, 0]).is_err());
    }
}
