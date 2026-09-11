//! Table seats, positions and action order.

use std::fmt;

use crate::Chips;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Position {
    Unknown,
    ButtonSmallBlind,
    Button,
    SmallBlind,
    BigBlind,
    Utg,
    Utg1,
    Lj,
    Hj,
    Co,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Unknown => "UNKNOWN",
            Self::ButtonSmallBlind => "BTN/SB",
            Self::Button => "BTN",
            Self::SmallBlind => "SB",
            Self::BigBlind => "BB",
            Self::Utg => "UTG",
            Self::Utg1 => "UTG+1",
            Self::Lj => "LJ",
            Self::Hj => "HJ",
            Self::Co => "CO",
        };
        f.write_str(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnteMode {
    None,
    Uniform,
    BigBlind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableConfig {
    pub table_size: usize,
    pub button: usize,
    pub small_blind: Chips,
    pub big_blind: Chips,
    pub ante: Chips,
    pub ante_mode: AnteMode,
    pub stacks: Vec<Chips>,
    pub dead_money: Chips,
}

impl TableConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(2..=8).contains(&self.table_size) {
            return Err(format!("table size must be 2..=8, got {}", self.table_size));
        }
        if self.stacks.len() != self.table_size {
            return Err(format!(
                "expected {} stacks, got {}",
                self.table_size,
                self.stacks.len()
            ));
        }
        if self.button >= self.table_size {
            return Err(format!("button seat out of range: {}", self.button));
        }
        if self.small_blind < 0 || self.big_blind < 0 || self.ante < 0 || self.dead_money < 0 {
            return Err("blinds, ante and dead money cannot be negative".to_string());
        }
        if self.big_blind < self.small_blind {
            return Err("big blind must be at least small blind".to_string());
        }
        if self.stacks.iter().any(|&stack| stack < 0) {
            return Err("stack cannot be negative".to_string());
        }
        Ok(())
    }

    pub fn small_blind_seat(&self) -> usize {
        if self.table_size == 2 {
            self.button
        } else {
            (self.button + 1) % self.table_size
        }
    }

    pub fn big_blind_seat(&self) -> usize {
        (self.small_blind_seat() + 1) % self.table_size
    }

    /// Preflop action starts immediately after the big blind. In heads-up,
    /// the button is also the small blind and therefore acts first preflop.
    pub fn preflop_order(&self) -> Vec<usize> {
        let first = (self.big_blind_seat() + 1) % self.table_size;
        (0..self.table_size)
            .map(|offset| (first + offset) % self.table_size)
            .collect()
    }

    /// Postflop action starts immediately after the button. In heads-up this
    /// is the big blind, which is correct for postflop play.
    pub fn postflop_order(&self) -> Vec<usize> {
        let first = (self.button + 1) % self.table_size;
        (0..self.table_size)
            .map(|offset| (first + offset) % self.table_size)
            .collect()
    }

    pub fn position_for_seat(&self, seat: usize) -> Result<Position, String> {
        if seat >= self.table_size {
            return Err(format!("seat out of range: {seat}"));
        }
        let offset = (seat + self.table_size - self.button) % self.table_size;
        if self.table_size == 2 {
            return Ok(if offset == 0 {
                Position::ButtonSmallBlind
            } else {
                Position::BigBlind
            });
        }

        let position = match self.table_size {
            3 => match offset {
                0 => Position::Button,
                1 => Position::SmallBlind,
                _ => Position::BigBlind,
            },
            4 => match offset {
                0 => Position::Button,
                1 => Position::SmallBlind,
                2 => Position::BigBlind,
                _ => Position::Co,
            },
            5 => match offset {
                0 => Position::Button,
                1 => Position::SmallBlind,
                2 => Position::BigBlind,
                3 => Position::Utg,
                _ => Position::Co,
            },
            6 => match offset {
                0 => Position::Button,
                1 => Position::SmallBlind,
                2 => Position::BigBlind,
                3 => Position::Utg,
                4 => Position::Hj,
                _ => Position::Co,
            },
            7 => match offset {
                0 => Position::Button,
                1 => Position::SmallBlind,
                2 => Position::BigBlind,
                3 => Position::Utg,
                4 => Position::Utg1,
                5 => Position::Hj,
                _ => Position::Co,
            },
            8 => match offset {
                0 => Position::Button,
                1 => Position::SmallBlind,
                2 => Position::BigBlind,
                3 => Position::Utg,
                4 => Position::Utg1,
                5 => Position::Lj,
                6 => Position::Hj,
                _ => Position::Co,
            },
            _ => unreachable!(),
        };
        Ok(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(table_size: usize, button: usize) -> TableConfig {
        TableConfig {
            table_size,
            button,
            small_blind: 500,
            big_blind: 1_000,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![100_000; table_size],
            dead_money: 0,
        }
    }

    #[test]
    fn eight_max_positions_follow_button() {
        let table = config(8, 0);
        assert_eq!(table.position_for_seat(0).unwrap(), Position::Button);
        assert_eq!(table.position_for_seat(1).unwrap(), Position::SmallBlind);
        assert_eq!(table.position_for_seat(2).unwrap(), Position::BigBlind);
        assert_eq!(table.position_for_seat(3).unwrap(), Position::Utg);
        assert_eq!(table.position_for_seat(7).unwrap(), Position::Co);
    }

    #[test]
    fn heads_up_has_button_as_small_blind() {
        let table = config(2, 0);
        assert_eq!(table.small_blind_seat(), 0);
        assert_eq!(table.big_blind_seat(), 1);
        assert_eq!(table.preflop_order(), vec![0, 1]);
        assert_eq!(table.postflop_order(), vec![1, 0]);
    }

    #[test]
    fn six_max_orders_are_cyclic() {
        let table = config(6, 3);
        assert_eq!(table.preflop_order(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(table.postflop_order(), vec![4, 5, 0, 1, 2, 3]);
    }
}
