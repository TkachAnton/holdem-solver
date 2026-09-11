//! Preflop hand classes, exact combinations and weighted ranges.
//!
//! Важный принцип: class-level strategy не заменяет exact combo model.
//! Для blockers и equity всегда сохраняются точные комбинации.

use std::collections::HashSet;

use holdem_cards::{card_mask, rank, suit, Card, DeckMask, RANKS};

pub type HandClassId = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Combo {
    pub cards: [Card; 2],
}

impl Combo {
    pub fn new(first: Card, second: Card) -> Result<Self, String> {
        if first == second {
            return Err("a combo cannot contain the same card twice".to_string());
        }
        if first >= 52 || second >= 52 {
            return Err("card code out of range".to_string());
        }
        Ok(Self {
            cards: [first, second],
        })
    }

    pub fn mask(self) -> DeckMask {
        card_mask(self.cards[0]) | card_mask(self.cards[1])
    }

    pub fn conflicts(self, dead: DeckMask) -> bool {
        self.mask() & dead != 0
    }

    pub fn class_id(self) -> HandClassId {
        class_id_from_combo(self)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WeightedCombo {
    pub combo: Combo,
    pub class_id: HandClassId,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HandClass {
    pub id: HandClassId,
    pub name: String,
    pub combos: Vec<Combo>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeightedRange {
    pub combos: Vec<WeightedCombo>,
}

impl WeightedRange {
    pub fn from_classes(classes: &[HandClass]) -> Self {
        let mut combos = Vec::new();
        for class in classes {
            for &combo in &class.combos {
                combos.push(WeightedCombo {
                    combo,
                    class_id: class.id,
                    weight: 1.0,
                });
            }
        }
        Self { combos }
    }

    pub fn legal_against_mask(&self, dead: DeckMask) -> Self {
        let combos = self
            .combos
            .iter()
            .filter(|entry| !entry.combo.conflicts(dead) && entry.weight > 0.0)
            .cloned()
            .collect();
        Self { combos }
    }

    pub fn total_weight(&self) -> f64 {
        self.combos.iter().map(|entry| entry.weight.max(0.0)).sum()
    }

    pub fn normalize(&mut self) {
        let total = self.total_weight();
        if total > 0.0 {
            for entry in &mut self.combos {
                entry.weight = entry.weight.max(0.0) / total;
            }
        }
    }
}

pub fn rank_index(value: char) -> Option<usize> {
    RANKS.iter().position(|&rank| rank as char == value)
}

pub fn rank_char(index: usize) -> Option<char> {
    RANKS.get(index).copied().map(char::from)
}

pub fn class_id_from_combo(combo: Combo) -> HandClassId {
    let first_rank = rank(combo.cards[0]);
    let second_rank = rank(combo.cards[1]);
    let high = first_rank.max(second_rank) as u16;
    let low = first_rank.min(second_rank) as u16;
    let suited = if high != low && suit(combo.cards[0]) == suit(combo.cards[1]) {
        1
    } else {
        0
    };

    high * 26 + low * 2 + suited
}

pub fn class_id_from_name(name: &str) -> Result<HandClassId, String> {
    let combos = combos_for_hand(name)?;
    combos
        .first()
        .copied()
        .map(class_id_from_combo)
        .ok_or_else(|| format!("hand has no combinations: {name}"))
}

pub fn combos_for_hand(name: &str) -> Result<Vec<Combo>, String> {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() != 2 && chars.len() != 3 {
        return Err(format!("invalid hand name: {name}"));
    }

    let first_rank = rank_index(chars[0]).ok_or_else(|| format!("invalid rank in {name}"))?;
    let second_rank = rank_index(chars[1]).ok_or_else(|| format!("invalid rank in {name}"))?;

    if first_rank == second_rank {
        if chars.len() != 2 {
            return Err(format!("pairs cannot have suited/off-suit suffix: {name}"));
        }

        let mut combos = Vec::with_capacity(6);
        for first_suit in 0..4u8 {
            for second_suit in (first_suit + 1)..4u8 {
                let first = (first_rank as u8) * 4 + first_suit;
                let second = (second_rank as u8) * 4 + second_suit;
                combos.push(Combo::new(first, second)?);
            }
        }
        return Ok(combos);
    }

    if chars.len() != 3 {
        return Err(format!("non-pair hand needs s/o suffix: {name}"));
    }

    let suffix = chars[2];
    if suffix != 's' && suffix != 'o' {
        return Err(format!("invalid hand suffix in {name}"));
    }

    let high = first_rank.max(second_rank);
    let low = first_rank.min(second_rank);
    let mut combos = Vec::new();

    if suffix == 's' {
        for current_suit in 0..4u8 {
            let first = high as u8 * 4 + current_suit;
            let second = low as u8 * 4 + current_suit;
            combos.push(Combo::new(first, second)?);
        }
    } else {
        for first_suit in 0..4u8 {
            for second_suit in 0..4u8 {
                if first_suit != second_suit {
                    let first = high as u8 * 4 + first_suit;
                    let second = low as u8 * 4 + second_suit;
                    combos.push(Combo::new(first, second)?);
                }
            }
        }
    }

    Ok(combos)
}

fn canonical_hand_name(high: usize, low: usize, suffix: Option<char>) -> Result<String, String> {
    let high_char = rank_char(high).ok_or_else(|| "invalid high rank".to_string())?;
    let low_char = rank_char(low).ok_or_else(|| "invalid low rank".to_string())?;
    Ok(match suffix {
        Some(suffix) => format!("{high_char}{low_char}{suffix}"),
        None => format!("{high_char}{low_char}"),
    })
}

fn expand_range_part(part: &str) -> Result<Vec<String>, String> {
    let trimmed = part.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let Some((start, end)) = trimmed.split_once('-') else {
        // Normalize the name through the combo generator. This also validates it.
        let combos = combos_for_hand(trimmed)?;
        let combo = combos
            .first()
            .copied()
            .ok_or_else(|| format!("empty hand: {trimmed}"))?;
        let high = rank(combo.cards[0]).max(rank(combo.cards[1]));
        let low = rank(combo.cards[0]).min(rank(combo.cards[1]));
        let suffix = if high == low {
            None
        } else if suit(combo.cards[0]) == suit(combo.cards[1]) {
            Some('s')
        } else {
            Some('o')
        };
        return Ok(vec![canonical_hand_name(high, low, suffix)?]);
    };

    let start_chars: Vec<char> = start.trim().chars().collect();
    let end_chars: Vec<char> = end.trim().chars().collect();
    if (start_chars.len() != 2 && start_chars.len() != 3)
        || (end_chars.len() != 2 && end_chars.len() != 3)
    {
        return Err(format!("invalid range part: {trimmed}"));
    }

    let start_hi = rank_index(start_chars[0]).ok_or_else(|| "invalid start rank".to_string())?;
    let start_lo = rank_index(start_chars[1]).ok_or_else(|| "invalid start rank".to_string())?;
    let end_hi = rank_index(end_chars[0]).ok_or_else(|| "invalid end rank".to_string())?;
    let end_lo = rank_index(end_chars[1]).ok_or_else(|| "invalid end rank".to_string())?;

    if start_hi == start_lo && end_hi == end_lo {
        let mut result = Vec::new();
        for current in (end_hi..=start_hi).rev() {
            result.push(canonical_hand_name(current, current, None)?);
        }
        return Ok(result);
    }

    if start_hi != end_hi || start_hi <= start_lo || end_hi <= end_lo {
        return Err(format!("unsupported or invalid range: {trimmed}"));
    }

    if start_chars.len() != 3 || end_chars.len() != 3 || start_chars[2] != end_chars[2] {
        return Err(format!("range suffixes must match: {trimmed}"));
    }

    let suffix = start_chars[2];
    if suffix != 's' && suffix != 'o' {
        return Err(format!("invalid range suffix: {trimmed}"));
    }

    let mut result = Vec::new();
    for current in (end_lo..=start_lo).rev() {
        result.push(canonical_hand_name(start_hi, current, Some(suffix))?);
    }
    Ok(result)
}

pub fn parse_range(range: &str) -> Result<Vec<HandClass>, String> {
    let mut classes = Vec::new();
    let mut seen = HashSet::new();

    for part in range.split(',') {
        for name in expand_range_part(part)? {
            if !seen.insert(name.clone()) {
                continue;
            }
            let combos = combos_for_hand(&name)?;
            let id = combos
                .first()
                .copied()
                .map(class_id_from_combo)
                .ok_or_else(|| format!("empty hand class: {name}"))?;
            classes.push(HandClass { id, name, combos });
        }
    }

    Ok(classes)
}

pub fn filter_combos_by_dead_cards(combos: &[Combo], dead: DeckMask) -> Vec<Combo> {
    combos
        .iter()
        .copied()
        .filter(|combo| !combo.conflicts(dead))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::{card_code, mask_from_cards};

    #[test]
    fn combo_counts_are_correct() {
        assert_eq!(combos_for_hand("AA").unwrap().len(), 6);
        assert_eq!(combos_for_hand("AKs").unwrap().len(), 4);
        assert_eq!(combos_for_hand("AKo").unwrap().len(), 12);
    }

    #[test]
    fn ranges_expand_in_descending_strength_order() {
        let classes = parse_range("AA-99, AKs-ATs, AKo").unwrap();
        let names: Vec<&str> = classes.iter().map(|class| class.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["AA", "KK", "QQ", "JJ", "TT", "99", "AKs", "AQs", "AJs", "ATs", "AKo"]
        );
    }

    #[test]
    fn classes_have_stable_ids() {
        let classes = parse_range("ATs, AKo, AA").unwrap();
        assert_eq!(classes[0].id, 329);
        assert_eq!(classes[1].id, 334);
        assert_eq!(classes[2].id, 336);
    }

    #[test]
    fn board_filter_removes_conflicting_combos() {
        let aa = combos_for_hand("AA").unwrap();
        let board =
            mask_from_cards(&[card_code('A', 's').unwrap(), card_code('A', 'h').unwrap()]).unwrap();
        assert_eq!(filter_combos_by_dead_cards(&aa, board).len(), 1);
    }

    #[test]
    fn range_duplicates_are_deduplicated() {
        let classes = parse_range("AA, AA, KK").unwrap();
        assert_eq!(classes.len(), 2);
    }
}
