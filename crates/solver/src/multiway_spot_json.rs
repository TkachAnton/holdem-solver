//! JSON job schema for high-level multiway Hold'em spots.
//!
//! The schema keeps the user-facing representation independent from domain
//! enums: actions, ante modes, ranges, cards and tree abstraction are parsed
//! explicitly and validated before a `MultiwayHoldemSpotConfig` is built.

use holdem_cards::{cards_from_str, mask_from_cards, DeckMask};
use holdem_domain::table::{AnteMode, TableConfig};
use holdem_domain::{Action, ActionSizes, Chips, PlayerId};
use holdem_ranges::{combos_for_hand, parse_range, Combo, WeightedCombo, WeightedRange};
use holdem_tree::action_abstraction::{ActionAbstraction, StreetSizing};
use holdem_tree::{ChanceConfig, ChanceOutcome, FullTreeBuildConfig, TreeBuildConfig};
use serde::{Deserialize, Serialize};

use crate::{MultiwayHoldemSpotAction, MultiwayHoldemSpotConfig, MultiwayHoldemSpotTreeConfig};

pub const MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION: u32 = 1;

fn default_max_nodes() -> usize {
    100_000
}

fn default_max_depth() -> usize {
    128
}

fn default_max_outcomes() -> usize {
    10_000
}

fn default_max_private_attempts() -> usize {
    10_000
}

fn default_worker_count() -> usize {
    1
}

fn default_reduction_batch_size() -> usize {
    1
}

fn default_checkpoint_interval() -> u64 {
    1_000
}

fn default_keep_checkpoints() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotJob {
    pub schema_version: u32,
    pub table: MultiwayHoldemSpotTableJson,
    #[serde(default)]
    pub dead_cards: String,
    #[serde(default)]
    pub history: Vec<MultiwayHoldemSpotActionJson>,
    /// One continuation range per table seat. These ranges are interpreted at
    /// the state after `history`; the parser does not infer action-conditioned
    /// range narrowing from the action sequence.
    pub ranges: Vec<MultiwayHoldemSpotRangeJson>,
    pub hero: MultiwayHoldemSpotHeroJson,
    pub tree: MultiwayHoldemSpotTreeJson,
    #[serde(default)]
    pub execution: MultiwayHoldemSpotExecutionJson,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotTableJson {
    pub table_size: usize,
    pub button: usize,
    pub small_blind: Chips,
    pub big_blind: Chips,
    #[serde(default)]
    pub ante: Chips,
    #[serde(default)]
    pub ante_mode: MultiwayHoldemSpotAnteModeJson,
    pub stacks: Vec<Chips>,
    #[serde(default)]
    pub dead_money: Chips,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiwayHoldemSpotAnteModeJson {
    None,
    Uniform,
    BigBlind,
}

impl Default for MultiwayHoldemSpotAnteModeJson {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotActionJson {
    pub player: PlayerId,
    #[serde(flatten)]
    pub action: MultiwayHoldemSpotActionKindJson,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MultiwayHoldemSpotActionKindJson {
    Fold,
    Check,
    Call,
    Bet { to: Chips },
    Raise { to: Chips },
    AllIn,
}

impl MultiwayHoldemSpotActionKindJson {
    fn to_action(&self) -> Action {
        match self {
            Self::Fold => Action::Fold,
            Self::Check => Action::Check,
            Self::Call => Action::Call,
            Self::Bet { to } => Action::Bet { to: *to },
            Self::Raise { to } => Action::Raise { to: *to },
            Self::AllIn => Action::AllIn,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotRangeJson {
    pub range: String,
    #[serde(default)]
    pub class_weights: Vec<MultiwayHoldemSpotClassWeightJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotClassWeightJson {
    pub hand: String,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotHeroJson {
    pub player: PlayerId,
    /// Either a class such as `AJo`/`JJ` or one exact combo such as `As Jd`.
    pub hand: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotTreeJson {
    #[serde(default)]
    pub mode: MultiwayHoldemSpotTreeModeJson,
    #[serde(default = "default_max_nodes")]
    pub max_nodes: usize,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    #[serde(default)]
    pub preflop: MultiwayHoldemSpotStreetJson,
    #[serde(default)]
    pub flop: MultiwayHoldemSpotStreetJson,
    #[serde(default)]
    pub turn: MultiwayHoldemSpotStreetJson,
    #[serde(default)]
    pub river: MultiwayHoldemSpotStreetJson,
    #[serde(default)]
    pub chance: MultiwayHoldemSpotChanceJson,
    #[serde(default)]
    pub postflop_order: Vec<PlayerId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiwayHoldemSpotTreeModeJson {
    Round,
    Full,
}

impl Default for MultiwayHoldemSpotTreeModeJson {
    fn default() -> Self {
        Self::Full
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotStreetJson {
    #[serde(default)]
    pub explicit_bet_to: Vec<Chips>,
    #[serde(default)]
    pub bet_fractions: Vec<f64>,
    #[serde(default)]
    pub explicit_raise_to: Vec<Chips>,
    #[serde(default)]
    pub raise_multipliers: Vec<f64>,
    #[serde(default)]
    pub include_all_in: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotChanceJson {
    #[serde(default)]
    pub flop: Vec<MultiwayHoldemSpotChanceOutcomeJson>,
    #[serde(default)]
    pub turn: Vec<MultiwayHoldemSpotChanceOutcomeJson>,
    #[serde(default)]
    pub river: Vec<MultiwayHoldemSpotChanceOutcomeJson>,
    #[serde(default)]
    pub enumerate_exact: bool,
    #[serde(default = "default_max_outcomes")]
    pub max_outcomes_per_node: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotChanceOutcomeJson {
    pub cards: String,
    pub probability: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiwayHoldemSpotExecutionJson {
    #[serde(default)]
    pub seed: u64,
    /// Required by the native runner unless overridden on the command line.
    #[serde(default)]
    pub target_iterations: u64,
    #[serde(default = "default_checkpoint_interval")]
    pub checkpoint_interval: u64,
    #[serde(default = "default_keep_checkpoints")]
    pub keep_checkpoints: usize,
    #[serde(default = "default_max_private_attempts")]
    pub max_private_attempts: usize,
    #[serde(default = "default_worker_count")]
    pub worker_count: usize,
    #[serde(default = "default_reduction_batch_size")]
    pub reduction_batch_size: usize,
    #[serde(default)]
    pub config_fingerprint: u64,
}

impl Default for MultiwayHoldemSpotExecutionJson {
    fn default() -> Self {
        Self {
            seed: 0,
            target_iterations: 0,
            checkpoint_interval: default_checkpoint_interval(),
            keep_checkpoints: default_keep_checkpoints(),
            max_private_attempts: default_max_private_attempts(),
            worker_count: default_worker_count(),
            reduction_batch_size: default_reduction_batch_size(),
            config_fingerprint: 0,
        }
    }
}

impl MultiwayHoldemSpotJob {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let job: Self = serde_json::from_str(json).map_err(|error| error.to_string())?;
        if job.schema_version != MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported multiway Hold'em spot schema version: {}",
                job.schema_version
            ));
        }
        Ok(job)
    }

    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn into_config(self) -> Result<MultiwayHoldemSpotConfig, String> {
        if self.schema_version != MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported multiway Hold'em spot schema version: {}",
                self.schema_version
            ));
        }
        let table = TableConfig {
            table_size: self.table.table_size,
            button: self.table.button,
            small_blind: self.table.small_blind,
            big_blind: self.table.big_blind,
            ante: self.table.ante,
            ante_mode: self.table.ante_mode.to_domain(),
            stacks: self.table.stacks,
            dead_money: self.table.dead_money,
        };
        table.validate()?;
        let dead_cards = parse_cards_mask(&self.dead_cards)?;
        let history = self
            .history
            .into_iter()
            .map(|entry| MultiwayHoldemSpotAction {
                player: entry.player,
                action: entry.action.to_action(),
            })
            .collect();
        let ranges = self
            .ranges
            .into_iter()
            .map(MultiwayHoldemSpotRangeJson::into_range)
            .collect::<Result<Vec<_>, _>>()?;
        let hero_hands = parse_hero_hands(&self.hero.hand)?;
        let tree = self.tree.into_tree_config()?;
        let execution = self.execution;
        let config = MultiwayHoldemSpotConfig {
            table,
            action_history: history,
            ranges,
            dead_cards,
            tree,
            hero_player: self.hero.player,
            hero_hands,
            hero_label: self.hero.label.unwrap_or(self.hero.hand),
            seed: execution.seed,
            config_fingerprint: execution.config_fingerprint,
            max_private_attempts: execution.max_private_attempts,
            worker_count: execution.worker_count,
            reduction_batch_size: execution.reduction_batch_size,
        };
        config.validate()?;
        Ok(config)
    }
}

impl MultiwayHoldemSpotAnteModeJson {
    fn to_domain(self) -> AnteMode {
        match self {
            Self::None => AnteMode::None,
            Self::Uniform => AnteMode::Uniform,
            Self::BigBlind => AnteMode::BigBlind,
        }
    }
}

impl MultiwayHoldemSpotRangeJson {
    fn into_range(self) -> Result<WeightedRange, String> {
        let classes = parse_range(&self.range)?;
        let mut class_weights = std::collections::HashMap::new();
        for override_weight in self.class_weights {
            if !override_weight.weight.is_finite() || override_weight.weight < 0.0 {
                return Err(format!(
                    "range class weight for {} must be finite and non-negative",
                    override_weight.hand
                ));
            }
            if class_weights
                .insert(override_weight.hand.clone(), override_weight.weight)
                .is_some()
            {
                return Err(format!(
                    "range contains duplicate class weight for {}",
                    override_weight.hand
                ));
            }
        }

        let mut combos = Vec::new();
        for class in classes {
            let multiplier = class_weights.get(&class.name).copied().unwrap_or(1.0);
            for combo in class.combos {
                combos.push(WeightedCombo {
                    combo,
                    class_id: class.id,
                    weight: multiplier,
                });
            }
        }
        if combos.iter().all(|entry| entry.weight <= 0.0) {
            return Err("range has no positive-weight combos".to_string());
        }
        Ok(WeightedRange { combos })
    }
}

impl MultiwayHoldemSpotTreeJson {
    fn into_tree_config(self) -> Result<MultiwayHoldemSpotTreeConfig, String> {
        let round = TreeBuildConfig {
            action_sizes: ActionSizes::default(),
            abstraction: Some(ActionAbstraction {
                preflop: self.preflop.into_sizing(),
                flop: self.flop.into_sizing(),
                turn: self.turn.into_sizing(),
                river: self.river.into_sizing(),
            }),
            max_nodes: self.max_nodes,
            max_depth: self.max_depth,
        };
        if matches!(self.mode, MultiwayHoldemSpotTreeModeJson::Round) {
            return Ok(MultiwayHoldemSpotTreeConfig::Round(round));
        }

        let chance = self.chance.into_chance_config()?;
        Ok(MultiwayHoldemSpotTreeConfig::Full(FullTreeBuildConfig {
            round,
            chance,
            postflop_order: self.postflop_order,
        }))
    }
}

impl MultiwayHoldemSpotStreetJson {
    fn into_sizing(self) -> StreetSizing {
        StreetSizing {
            explicit_bet_to: self.explicit_bet_to,
            bet_fractions: self.bet_fractions,
            explicit_raise_to: self.explicit_raise_to,
            raise_multipliers: self.raise_multipliers,
            include_all_in: self.include_all_in,
        }
    }
}

impl MultiwayHoldemSpotChanceJson {
    fn into_chance_config(self) -> Result<ChanceConfig, String> {
        Ok(ChanceConfig {
            flop: optional_outcomes(self.flop)?,
            turn: optional_outcomes(self.turn)?,
            river: optional_outcomes(self.river)?,
            enumerate_exact: self.enumerate_exact,
            max_outcomes_per_node: self.max_outcomes_per_node,
        })
    }
}

fn optional_outcomes(
    outcomes: Vec<MultiwayHoldemSpotChanceOutcomeJson>,
) -> Result<Option<Vec<ChanceOutcome>>, String> {
    if outcomes.is_empty() {
        return Ok(None);
    }
    outcomes
        .into_iter()
        .map(|outcome| {
            let cards = cards_from_str(&outcome.cards).map_err(|error| error.to_string())?;
            Ok(ChanceOutcome::new(cards, outcome.probability))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(Some)
}

fn parse_cards_mask(text: &str) -> Result<DeckMask, String> {
    if text.trim().is_empty() {
        return Ok(0);
    }
    mask_from_cards(&cards_from_str(text).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

fn parse_hero_hands(text: &str) -> Result<Vec<Combo>, String> {
    let cards: Vec<_> = text.split_whitespace().collect();
    if cards.len() == 2 {
        let parsed = cards_from_str(text).map_err(|error| error.to_string())?;
        return Ok(vec![Combo::new(parsed[0], parsed[1])?]);
    }
    combos_for_hand(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_job() -> MultiwayHoldemSpotJob {
        MultiwayHoldemSpotJob {
            schema_version: MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION,
            table: MultiwayHoldemSpotTableJson {
                table_size: 3,
                button: 0,
                small_blind: 1,
                big_blind: 2,
                ante: 0,
                ante_mode: MultiwayHoldemSpotAnteModeJson::None,
                stacks: vec![100, 100, 100],
                dead_money: 0,
            },
            dead_cards: String::new(),
            history: Vec::new(),
            ranges: vec![
                MultiwayHoldemSpotRangeJson {
                    range: "AJo".to_string(),
                    class_weights: Vec::new(),
                },
                MultiwayHoldemSpotRangeJson {
                    range: "KK".to_string(),
                    class_weights: Vec::new(),
                },
                MultiwayHoldemSpotRangeJson {
                    range: "QQ".to_string(),
                    class_weights: Vec::new(),
                },
            ],
            hero: MultiwayHoldemSpotHeroJson {
                player: 0,
                hand: "AJo".to_string(),
                label: None,
            },
            tree: MultiwayHoldemSpotTreeJson {
                mode: MultiwayHoldemSpotTreeModeJson::Round,
                max_nodes: 10_000,
                max_depth: 32,
                ..MultiwayHoldemSpotTreeJson::default()
            },
            execution: MultiwayHoldemSpotExecutionJson::default(),
        }
    }

    #[test]
    fn json_job_parses_hero_class_and_ranges() {
        let job = sample_job();
        let json = job.to_json().unwrap();
        let decoded = MultiwayHoldemSpotJob::from_json(&json).unwrap();
        let config = decoded.into_config().unwrap();
        assert_eq!(config.hero_hands.len(), 12);
        assert_eq!(config.ranges[0].combos.len(), 12);
        assert_eq!(config.hero_label, "AJo");
    }

    #[test]
    fn json_history_action_uses_player_and_flattened_kind() {
        let action: MultiwayHoldemSpotActionJson =
            serde_json::from_str(r#"{"player":5,"kind":"raise","to":500}"#).unwrap();
        assert_eq!(action.player, 5);
        assert!(matches!(
            action.action,
            MultiwayHoldemSpotActionKindJson::Raise { to: 500 }
        ));
    }

    #[test]
    fn checked_in_8max_ajo_example_builds_history_state() {
        let json = include_str!("../../../docs/examples/multiway_spot_8max_ajo.json");
        let config = MultiwayHoldemSpotJob::from_json(json)
            .unwrap()
            .into_config()
            .unwrap();
        let state = config.state_after_history().unwrap();
        assert_eq!(state.actor, Some(1));
        assert_eq!(config.hero_hands.len(), 12);
    }

    #[test]
    fn json_job_supports_exact_hero_combo_and_weight_override() {
        let mut job = sample_job();
        job.hero.hand = "As Jd".to_string();
        job.ranges[0]
            .class_weights
            .push(MultiwayHoldemSpotClassWeightJson {
                hand: "AJo".to_string(),
                weight: 0.25,
            });
        let config = job.into_config().unwrap();
        assert_eq!(config.hero_hands.len(), 1);
        assert_eq!(config.ranges[0].combos[0].weight, 0.25);
    }
}
