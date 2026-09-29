//! Street-specific action abstraction.
//!
//! The abstraction converts pot fractions and raise multipliers into the
//! absolute targets expected by the domain state machine.

use holdem_domain::{ActionSizes, Chips, GameState, Street};

#[derive(Debug, Clone, PartialEq)]
pub struct StreetSizing {
    pub explicit_bet_to: Vec<Chips>,
    pub bet_fractions: Vec<f64>,
    pub explicit_raise_to: Vec<Chips>,
    pub raise_multipliers: Vec<f64>,
    pub include_all_in: bool,
    /// T5.1/D-024: позиционные рейз-сайзинги улицы (RFI/3-бет конвенции):
    /// None — глобальный explicit_raise_to (прежнее поведение, пины не тронуты);
    /// Some — актору с известной позицией берётся его список, позиция
    /// без записи — ошибка конфигурации (жёстко, D-013).
    pub raise_to_by_position: Option<Vec<(holdem_domain::table::Position, Vec<Chips>)>>,
}

impl Default for StreetSizing {
    fn default() -> Self {
        Self {
            explicit_bet_to: Vec::new(),
            bet_fractions: Vec::new(),
            explicit_raise_to: Vec::new(),
            raise_to_by_position: None,
            raise_multipliers: Vec::new(),
            include_all_in: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActionAbstraction {
    pub preflop: StreetSizing,
    pub flop: StreetSizing,
    pub turn: StreetSizing,
    pub river: StreetSizing,
}

impl StreetSizing {
    /// Deterministic fingerprint for checkpoint/config compatibility checks.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = FNV_OFFSET;
        hash = update_hash(hash, self.explicit_bet_to.len() as u64);
        for &value in &self.explicit_bet_to {
            hash = update_hash(hash, value as u64);
        }
        hash = update_hash(hash, self.bet_fractions.len() as u64);
        for &value in &self.bet_fractions {
            hash = update_hash(hash, value.to_bits());
        }
        hash = update_hash(hash, self.explicit_raise_to.len() as u64);
        for &value in &self.explicit_raise_to {
            hash = update_hash(hash, value as u64);
        }
        hash = update_hash(hash, self.raise_multipliers.len() as u64);
        for &value in &self.raise_multipliers {
            hash = update_hash(hash, value.to_bits());
        }
        update_hash(hash, self.include_all_in as u64)
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0001_0000_01b3;

fn update_hash(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

impl ActionAbstraction {
    /// Deterministic fingerprint for the complete street-specific abstraction.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = update_hash(FNV_OFFSET, 1);
        for sizing in [&self.preflop, &self.flop, &self.turn, &self.river] {
            hash = update_hash(hash, sizing.fingerprint());
        }
        hash
    }

    pub fn sizing_for(&self, street: Street) -> &StreetSizing {
        match street {
            Street::Preflop => &self.preflop,
            Street::Flop => &self.flop,
            Street::Turn => &self.turn,
            Street::River => &self.river,
        }
    }

    pub fn action_sizes(&self, state: &GameState) -> Result<ActionSizes, String> {
        let sizing = self.sizing_for(state.street);
        let player = state
            .actor
            .and_then(|actor| state.players.iter().find(|player| player.seat == actor))
            .ok_or_else(|| "action abstraction requires a current actor".to_string())?;

        let mut result = ActionSizes {
            include_all_in: sizing.include_all_in,
            ..ActionSizes::default()
        };
        let maximum = player.committed_street + player.stack_remaining;

        if state.current_bet == 0 {
            result.bet_to.extend(
                sizing
                    .explicit_bet_to
                    .iter()
                    .copied()
                    .filter(|&target| target > 0 && target <= maximum),
            );
            for &fraction in &sizing.bet_fractions {
                if !fraction.is_finite() || fraction <= 0.0 {
                    return Err("bet fractions must be finite and positive".to_string());
                }
                let amount = round_chips(state.pot as f64 * fraction)?;
                let target = player.committed_street.saturating_add(amount);
                if target > 0 && target <= maximum {
                    result.bet_to.push(target);
                }
            }
            dedup_sort(&mut result.bet_to);
        } else {
            // T5.1: позиционная карта — жёстко на отсутствие записи для
            // позиции актора (конфигурация ошибочна); достижимость цели в
            // конкретном узле (current_bet/maximum/мин-инкремент) — мягкий
            // фильтр, как исторически: глобальный список легален на улице,
            // но не обязан быть достижимым в каждом узле.
            let raise_source: Vec<Chips> = match &sizing.raise_to_by_position {
                Some(map) => {
                    let position = position_of_actor(state).ok_or_else(|| {
                        "positional sizing requires an actor with a known position".to_string()
                    })?;
                    let (_, targets) = map
                        .iter()
                        .find(|(candidate, _)| *candidate == position)
                        .ok_or_else(|| {
                            format!("positional sizing has no entry for position {position:?}")
                        })?;
                    targets.clone()
                }
                None => sizing.explicit_raise_to.clone(),
            };
            for target in raise_source {
                let increment = target - state.current_bet;
                if target > state.current_bet
                    && target <= maximum
                    && increment >= state.min_raise_increment
                {
                    result.raise_to.push(target);
                }
            }
            for &multiplier in &sizing.raise_multipliers {
                if !multiplier.is_finite() || multiplier <= 1.0 {
                    return Err("raise multipliers must be finite and greater than one".to_string());
                }
                let target = round_chips(state.current_bet as f64 * multiplier)?;
                if target > state.current_bet && target <= maximum {
                    result.raise_to.push(target);
                }
            }
            dedup_sort(&mut result.raise_to);
        }

        Ok(result)
    }
}

fn position_of_actor(state: &GameState) -> Option<holdem_domain::table::Position> {
    let actor = state.actor?;
    state
        .players
        .iter()
        .find(|player| player.seat == actor)
        .map(|player| player.position)
}

fn round_chips(value: f64) -> Result<Chips, String> {
    if !value.is_finite() || value < 0.0 || value > Chips::MAX as f64 {
        return Err("action target is outside chip range".to_string());
    }
    Ok(value.round() as Chips)
}

fn dedup_sort(values: &mut Vec<Chips>) {
    values.sort_unstable();
    values.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_domain::{GameState, PlayerState};

    fn state(current_bet: Chips, pot: Chips) -> GameState {
        let mut players = vec![
            PlayerState::new(0, 10_000).unwrap(),
            PlayerState::new(1, 10_000).unwrap(),
        ];
        players[0].committed_total = current_bet;
        players[0].committed_street = current_bet;
        let mut state =
            GameState::new(2, Street::Flop, Vec::new(), players, pot - current_bet).unwrap();
        state
            .configure_betting(1, current_bet, current_bet.max(1), vec![1])
            .unwrap();
        state
    }

    #[test]
    fn pot_fraction_becomes_absolute_bet_target() {
        let state = state(0, 1_000);
        let abstraction = ActionAbstraction {
            flop: StreetSizing {
                bet_fractions: vec![0.5, 1.0],
                ..StreetSizing::default()
            },
            ..ActionAbstraction::default()
        };
        let sizes = abstraction.action_sizes(&state).unwrap();
        assert_eq!(sizes.bet_to, vec![500, 1_000]);
    }

    #[test]
    fn raise_multiplier_becomes_absolute_raise_target() {
        let state = state(200, 1_000);
        let abstraction = ActionAbstraction {
            flop: StreetSizing {
                raise_multipliers: vec![2.0, 3.0],
                ..StreetSizing::default()
            },
            ..ActionAbstraction::default()
        };
        let sizes = abstraction.action_sizes(&state).unwrap();
        assert_eq!(sizes.raise_to, vec![400, 600]);
    }

    #[test]
    fn fingerprints_are_deterministic_and_change_with_sizing() {
        let base = ActionAbstraction::default();
        let same = base.clone();
        let changed = ActionAbstraction {
            flop: StreetSizing {
                bet_fractions: vec![0.5],
                ..StreetSizing::default()
            },
            ..ActionAbstraction::default()
        };
        assert_eq!(base.fingerprint(), same.fingerprint());
        assert_ne!(base.fingerprint(), changed.fingerprint());
        assert_ne!(base.flop.fingerprint(), changed.flop.fingerprint());
    }

    #[test]
    fn invalid_sizing_is_rejected() {
        let state = state(0, 1_000);
        let abstraction = ActionAbstraction {
            flop: StreetSizing {
                bet_fractions: vec![0.0],
                ..StreetSizing::default()
            },
            ..ActionAbstraction::default()
        };
        assert!(abstraction.action_sizes(&state).is_err());
    }

    fn positional_state() -> GameState {
        use holdem_domain::setup::build_preflop_state;
        use holdem_domain::table::{AnteMode, TableConfig};
        let table = TableConfig {
            table_size: 3,
            button: 0,
            small_blind: 1,
            big_blind: 2,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![20000, 20000, 20000],
            dead_money: 0,
        };
        build_preflop_state(&table).unwrap()
    }

    #[test]
    fn positional_raise_sizing_selects_actor_entry() {
        use holdem_domain::table::Position;
        let state = positional_state();
        let sizing = StreetSizing {
            raise_to_by_position: Some(vec![
                (Position::Button, vec![500]),
                (Position::SmallBlind, vec![600]),
                (Position::BigBlind, vec![440]),
            ]),
            ..StreetSizing::default()
        };
        let abstraction = ActionAbstraction {
            preflop: sizing,
            ..ActionAbstraction::default()
        };
        let sizes = abstraction.action_sizes(&state).unwrap();
        assert_eq!(sizes.raise_to, vec![500]);
    }

    #[test]
    fn positional_raise_sizing_missing_entry_is_error() {
        use holdem_domain::table::Position;
        let state = positional_state();
        let sizing = StreetSizing {
            raise_to_by_position: Some(vec![(Position::BigBlind, vec![440])]),
            ..StreetSizing::default()
        };
        let abstraction = ActionAbstraction {
            preflop: sizing,
            ..ActionAbstraction::default()
        };
        let error = abstraction.action_sizes(&state).unwrap_err();
        assert!(error.contains("no entry"));
    }

    #[test]
    fn unreachable_global_raise_target_is_filtered_softly() {
        // T5.1-семантика: глобальный список легален на улице; цели,
        // недостижимые в конкретном узле (<= current_bet), фильтруются
        // мягко — как исторически. Жёсткость только у позиционной карты
        // («нет записи для позиции»).
        let state = positional_state();
        // BTN открывает: current_bet = 200 (BB). Цель 150 недостижима.
        let mut state = state;
        use holdem_domain::Action;
        // BTN raise to 500 -> current_bet 500 (после SB/BB? нет: 3-max, BTN
        // первый; для current_bet=500 применим raise на месте)
        state.apply_action(Action::Raise { to: 500 }).unwrap();
        // SB fold, BB call -> префлоп закрыт? Для теста фильтра достаточно
        // остаться в раунде: проверим фильтр на узле с current_bet=500.
        let sizing = StreetSizing {
            explicit_raise_to: vec![150, 400, 1200],
            ..StreetSizing::default()
        };
        let abstraction = ActionAbstraction {
            preflop: sizing,
            ..ActionAbstraction::default()
        };
        // Актор после рейза BTN: SB (seat 1). Глобальный список:
        // 150 <= 500 (недостижим — отфильтрован), 400 <= 500 (отфильтрован),
        // 900 > 500 и инкремент 400 >= min_raise — легален.
        let sizes = abstraction.action_sizes(&state).unwrap();
        assert_eq!(sizes.raise_to, vec![1200]);
    }
}
