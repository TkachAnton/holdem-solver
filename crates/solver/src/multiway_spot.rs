//! High-level preflop multiway Hold'em spot adapter.
//!
//! This layer turns a table configuration plus an observed action history into
//! a continuation `GameTree`. It intentionally does not infer post-action
//! ranges from the history: callers must pass ranges appropriate for the
//! selected starting point. The underlying batch solver still owns exact
//! blockers, ChipEV settlement, sampling and convergence diagnostics.

use holdem_cards::DeckMask;
use holdem_domain::setup::build_preflop_state;
use holdem_domain::table::TableConfig;
use holdem_domain::{Action, GameState, PlayerId};
use holdem_ranges::{combos_for_hand, Combo, WeightedRange};
use holdem_tree::{FullTreeBuildConfig, GameTree, LeafKind, TreeBuildConfig, TreeBuilder};
use serde::Serialize;

use crate::{
    multiway_holdem_tree_fingerprint, ActionExport, MultiwayBatchActionReport,
    MultiwayBatchStrategyReport, MultiwayHoldemBatchSolver,
};

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSpotAction {
    pub player: PlayerId,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub enum MultiwayHoldemSpotTreeConfig {
    /// Builds only the current betting round for inspection. It contains
    /// `RoundComplete` leaves and therefore is not sufficient for ChipEV
    /// solving; use `Full` when terminal showdown utility is required.
    Round(TreeBuildConfig),
    /// Builds the current round plus configured public-card transitions.
    Full(FullTreeBuildConfig),
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSpotConfig {
    pub table: TableConfig,
    /// Actions already observed before the hero decision. The sequence is
    /// checked against the domain state's actual actor at every step.
    pub action_history: Vec<MultiwayHoldemSpotAction>,
    /// Ranges for every table seat. For a continuation spot these should be
    /// conditioned on the observed history; the adapter does not guess them.
    pub ranges: Vec<WeightedRange>,
    pub dead_cards: DeckMask,
    pub tree: MultiwayHoldemSpotTreeConfig,
    pub hero_player: PlayerId,
    /// Exact hero combos to aggregate, for example all twelve combos from
    /// `combos_for_hand("AJo")`.
    pub hero_hands: Vec<Combo>,
    pub hero_label: String,
    pub seed: u64,
    pub config_fingerprint: u64,
    pub max_private_attempts: usize,
    pub worker_count: usize,
    pub reduction_batch_size: usize,
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSpotHeroHandReport {
    pub private_hand: Combo,
    pub visits: u64,
    pub actions: Vec<MultiwayBatchActionReport>,
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSpotHeroReport {
    pub player: PlayerId,
    pub label: String,
    pub public_node: usize,
    pub requested_hands: usize,
    pub observed_hands: Vec<MultiwayHoldemSpotHeroHandReport>,
    pub missing_hands: Vec<Combo>,
    pub observed_weight: f64,
    pub requested_weight: f64,
    pub actions: Vec<MultiwayBatchActionReport>,
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSpotResult {
    pub tree_fingerprint: u64,
    pub strategy_report: MultiwayBatchStrategyReport,
    pub hero: MultiwayHoldemSpotHeroReport,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayHoldemSpotResult {
    schema_version: u32,
    format: &'static str,
    tree_fingerprint: u64,
    strategy_report: serde_json::Value,
    hero: JsonMultiwayHoldemSpotHeroReport,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayHoldemSpotHeroReport {
    player: PlayerId,
    label: String,
    public_node: usize,
    requested_hands: usize,
    observed_hands: Vec<JsonMultiwayHoldemSpotHeroHandReport>,
    missing_hands: Vec<[u8; 2]>,
    observed_weight: f64,
    requested_weight: f64,
    actions: Vec<JsonMultiwayHoldemSpotActionReport>,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayHoldemSpotHeroHandReport {
    private_cards: [u8; 2],
    visits: u64,
    actions: Vec<JsonMultiwayHoldemSpotActionReport>,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayHoldemSpotActionReport {
    action: ActionExport,
    frequency: f64,
    positive_regret: f64,
}

impl MultiwayHoldemSpotResult {
    pub fn to_json(&self) -> Result<String, String> {
        let strategy_report = serde_json::from_str(&self.strategy_report.to_json()?)
            .map_err(|error| error.to_string())?;
        let hero = JsonMultiwayHoldemSpotHeroReport {
            player: self.hero.player,
            label: self.hero.label.clone(),
            public_node: self.hero.public_node,
            requested_hands: self.hero.requested_hands,
            observed_hands: self
                .hero
                .observed_hands
                .iter()
                .map(|hand| JsonMultiwayHoldemSpotHeroHandReport {
                    private_cards: hand.private_hand.cards,
                    visits: hand.visits,
                    actions: hand.actions.iter().map(json_spot_action).collect(),
                })
                .collect(),
            missing_hands: self
                .hero
                .missing_hands
                .iter()
                .map(|hand| hand.cards)
                .collect(),
            observed_weight: self.hero.observed_weight,
            requested_weight: self.hero.requested_weight,
            actions: self.hero.actions.iter().map(json_spot_action).collect(),
        };
        serde_json::to_string_pretty(&JsonMultiwayHoldemSpotResult {
            schema_version: crate::MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION,
            format: "multiway_holdem_spot_result",
            tree_fingerprint: self.tree_fingerprint,
            strategy_report,
            hero,
        })
        .map_err(|error| error.to_string())
    }
}

fn json_spot_action(action: &MultiwayBatchActionReport) -> JsonMultiwayHoldemSpotActionReport {
    JsonMultiwayHoldemSpotActionReport {
        action: ActionExport::from_action(&action.action),
        frequency: action.frequency,
        positive_regret: action.positive_regret,
    }
}

impl MultiwayHoldemSpotConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(3..=8).contains(&self.table.table_size) {
            return Err("multiway spot requires a 3-8 player table".to_string());
        }
        if self.ranges.len() != self.table.table_size {
            return Err(format!(
                "spot has {} ranges, expected {}",
                self.ranges.len(),
                self.table.table_size
            ));
        }
        if self.hero_player >= self.table.table_size {
            return Err(format!(
                "hero player {} is outside table size {}",
                self.hero_player, self.table.table_size
            ));
        }
        if self.hero_hands.is_empty() {
            return Err("spot hero_hands cannot be empty".to_string());
        }
        if self.max_private_attempts == 0 {
            return Err("spot max_private_attempts must be positive".to_string());
        }
        if self.worker_count == 0 {
            return Err("spot worker_count must be positive".to_string());
        }
        if self.reduction_batch_size == 0 {
            return Err("spot reduction_batch_size must be positive".to_string());
        }
        let mut unique = std::collections::HashSet::new();
        for &hand in &self.hero_hands {
            if !unique.insert(hand) {
                return Err("spot hero_hands contains a duplicate combo".to_string());
            }
            if hand.conflicts(self.dead_cards) {
                return Err("spot hero hand conflicts with dead cards".to_string());
            }
            if !self.ranges[self.hero_player]
                .combos
                .iter()
                .any(|entry| entry.combo == hand && entry.weight > 0.0)
            {
                return Err(format!(
                    "hero combo {:?} is absent or has zero weight in hero range",
                    hand.cards
                ));
            }
        }
        Ok(())
    }

    /// Convenience constructor for a class such as `AJo`, `KQs` or `JJ`.
    pub fn hero_class_combos(hand_class: &str) -> Result<Vec<Combo>, String> {
        combos_for_hand(hand_class)
    }

    pub fn state_after_history(&self) -> Result<GameState, String> {
        self.validate()?;
        let mut state = build_preflop_state(&self.table)?;
        for (index, observed) in self.action_history.iter().enumerate() {
            if state.terminal.is_some() {
                return Err(format!(
                    "spot action history continues after terminal action at index {index}"
                ));
            }
            if state.actor != Some(observed.player) {
                return Err(format!(
                    "spot history action {index} belongs to player {}, expected actor {:?}",
                    observed.player, state.actor
                ));
            }
            state.apply_action(observed.action.clone())?;
        }
        if state.terminal.is_some() {
            return Err("spot history ends at a terminal state, not a hero decision".to_string());
        }
        if state.actor.is_none() {
            return Err("spot history ends between betting rounds without an actor".to_string());
        }
        if state.actor != Some(self.hero_player) {
            return Err(format!(
                "spot history ends at actor {:?}, not hero {}",
                state.actor, self.hero_player
            ));
        }
        Ok(state)
    }

    pub fn build_tree(&self) -> Result<GameTree, String> {
        let state = self.state_after_history()?;
        match &self.tree {
            MultiwayHoldemSpotTreeConfig::Round(config) => TreeBuilder::build_round(state, config),
            MultiwayHoldemSpotTreeConfig::Full(config) => {
                let mut config = config.clone();
                if config.postflop_order.is_empty() {
                    config.postflop_order = self.table.postflop_order();
                }
                TreeBuilder::build_full(state, &config)
            }
        }
    }

    pub fn build_solver(&self) -> Result<(GameTree, MultiwayHoldemBatchSolver), String> {
        let tree = self.build_tree()?;
        if tree
            .leaf_nodes()
            .any(|node| matches!(node.leaf.as_ref(), Some(LeafKind::RoundComplete)))
        {
            return Err(
                "spot solver tree contains RoundComplete leaves; use Full tree config with terminal streets"
                    .to_string(),
            );
        }
        let config_fingerprint = if self.config_fingerprint == 0 {
            multiway_holdem_tree_fingerprint(&tree)
        } else {
            self.config_fingerprint
        };
        let solver = MultiwayHoldemBatchSolver::new(
            tree.clone(),
            self.ranges.clone(),
            self.dead_cards,
            self.seed,
            config_fingerprint,
            self.max_private_attempts,
        )?;
        Ok((tree, solver))
    }

    pub fn result_from_solver(
        &self,
        tree: &GameTree,
        solver: &MultiwayHoldemBatchSolver,
        utility_samples: usize,
    ) -> Result<MultiwayHoldemSpotResult, String> {
        let strategy_report = solver.strategy_report(utility_samples, self.max_private_attempts)?;
        let hero = aggregate_hero_report(
            self,
            tree.root,
            &strategy_report,
            &self.ranges[self.hero_player],
        )?;
        Ok(MultiwayHoldemSpotResult {
            tree_fingerprint: multiway_holdem_tree_fingerprint(tree),
            strategy_report,
            hero,
        })
    }

    pub fn solve(
        &self,
        iterations: u64,
        utility_samples: usize,
    ) -> Result<MultiwayHoldemSpotResult, String> {
        if iterations == 0 {
            return Err("spot iterations must be positive".to_string());
        }
        let (tree, mut solver) = self.build_solver()?;
        solver.run_parallel(iterations, self.worker_count, self.reduction_batch_size)?;
        self.result_from_solver(&tree, &solver, utility_samples)
    }
}

fn aggregate_hero_report(
    config: &MultiwayHoldemSpotConfig,
    public_node: usize,
    report: &MultiwayBatchStrategyReport,
    hero_range: &WeightedRange,
) -> Result<MultiwayHoldemSpotHeroReport, String> {
    let requested_weight: f64 = config
        .hero_hands
        .iter()
        .map(|hand| {
            hero_range
                .combos
                .iter()
                .filter(|entry| entry.combo == *hand && entry.weight > 0.0)
                .map(|entry| entry.weight)
                .sum::<f64>()
        })
        .sum();
    if requested_weight <= 0.0 || !requested_weight.is_finite() {
        return Err("hero requested hands have no positive range weight".to_string());
    }

    let mut observed_hands = Vec::new();
    let mut missing_hands = Vec::new();
    for &hero_hand in &config.hero_hands {
        if let Some(infoset) = report.infosets.iter().find(|infoset| {
            infoset.player == config.hero_player
                && infoset.public_node == public_node
                && infoset.private_hand == hero_hand
        }) {
            observed_hands.push(MultiwayHoldemSpotHeroHandReport {
                private_hand: hero_hand,
                visits: infoset.visits,
                actions: infoset.actions.clone(),
            });
        } else {
            missing_hands.push(hero_hand);
        }
    }
    if observed_hands.is_empty() {
        return Err(
            "no requested hero combo was visited; increase iterations or private sampling coverage"
                .to_string(),
        );
    }

    let observed_weight: f64 = observed_hands
        .iter()
        .map(|hand| {
            hero_range
                .combos
                .iter()
                .filter(|entry| entry.combo == hand.private_hand && entry.weight > 0.0)
                .map(|entry| entry.weight)
                .sum::<f64>()
        })
        .sum();
    let first_actions = observed_hands[0].actions.clone();
    let mut actions = first_actions
        .iter()
        .map(|action| MultiwayBatchActionReport {
            action: action.action.clone(),
            frequency: 0.0,
            positive_regret: 0.0,
        })
        .collect::<Vec<_>>();
    for hand in &observed_hands {
        if hand.actions.len() != actions.len()
            || hand
                .actions
                .iter()
                .zip(actions.iter())
                .any(|(left, right)| left.action != right.action)
        {
            return Err(
                "hero information sets do not share the same action abstraction".to_string(),
            );
        }
        let weight = hero_range
            .combos
            .iter()
            .filter(|entry| entry.combo == hand.private_hand && entry.weight > 0.0)
            .map(|entry| entry.weight)
            .sum::<f64>();
        for (aggregate, action) in actions.iter_mut().zip(&hand.actions) {
            aggregate.frequency += weight * action.frequency;
            aggregate.positive_regret += weight * action.positive_regret;
        }
    }
    for action in &mut actions {
        action.frequency /= observed_weight;
        action.positive_regret /= observed_weight;
    }

    Ok(MultiwayHoldemSpotHeroReport {
        player: config.hero_player,
        label: config.hero_label.clone(),
        public_node,
        requested_hands: config.hero_hands.len(),
        observed_hands,
        missing_hands,
        observed_weight,
        requested_weight,
        actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_domain::table::AnteMode;
    use holdem_ranges::WeightedCombo;
    use holdem_tree::{ChanceConfig, ChanceOutcome};

    fn combo(text: &str) -> Combo {
        let cards = holdem_cards::cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    fn range(entries: &[&str]) -> WeightedRange {
        WeightedRange {
            combos: entries
                .iter()
                .map(|text| {
                    let combo = combo(text);
                    WeightedCombo {
                        combo,
                        class_id: combo.class_id(),
                        weight: 1.0,
                    }
                })
                .collect(),
        }
    }

    fn config() -> MultiwayHoldemSpotConfig {
        let table = TableConfig {
            table_size: 3,
            button: 0,
            small_blind: 1,
            big_blind: 2,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![100, 100, 100],
            dead_money: 0,
        };
        let mut hero_range = range(&["As Jd"]);
        hero_range.combos.push(WeightedCombo {
            combo: combo("Ah Jc"),
            class_id: combo("Ah Jc").class_id(),
            weight: 1.0,
        });
        MultiwayHoldemSpotConfig {
            table,
            action_history: Vec::new(),
            ranges: vec![hero_range, range(&["Kc Kd"]), range(&["Qs Qh"])],
            dead_cards: 0,
            tree: MultiwayHoldemSpotTreeConfig::Full(FullTreeBuildConfig {
                round: TreeBuildConfig {
                    action_sizes: holdem_domain::ActionSizes {
                        bet_to: Vec::new(),
                        raise_to: vec![8, 12],
                        include_all_in: false,
                    },
                    abstraction: None,
                    max_nodes: 100_000,
                    max_depth: 32,
                },
                chance: ChanceConfig {
                    flop: Some(vec![ChanceOutcome::new(
                        holdem_cards::cards_from_str("2s 3d 4c").unwrap(),
                        1.0,
                    )]),
                    turn: Some(vec![ChanceOutcome::new(
                        holdem_cards::cards_from_str("5h").unwrap(),
                        1.0,
                    )]),
                    river: Some(vec![ChanceOutcome::new(
                        holdem_cards::cards_from_str("6s").unwrap(),
                        1.0,
                    )]),
                    enumerate_exact: false,
                    max_outcomes_per_node: 10,
                },
                postflop_order: Vec::new(),
            }),
            hero_player: 0,
            hero_hands: vec![combo("As Jd"), combo("Ah Jc")],
            hero_label: "AJo".to_string(),
            seed: 7,
            config_fingerprint: 0,
            max_private_attempts: 100,
            worker_count: 1,
            reduction_batch_size: 1,
        }
    }

    #[test]
    fn validates_history_and_builds_continuation_tree() {
        let mut config = config();
        config.action_history = vec![MultiwayHoldemSpotAction {
            player: 1,
            action: Action::Call,
        }];
        assert!(config.state_after_history().is_err());
        config.action_history.clear();
        let tree = config.build_tree().unwrap();
        assert_eq!(tree.root, 0);
        assert!(tree.node(tree.root).unwrap().state.actor == Some(0));
    }

    #[test]
    fn class_helper_expands_ajo_to_twelve_combos() {
        assert_eq!(
            MultiwayHoldemSpotConfig::hero_class_combos("AJo")
                .unwrap()
                .len(),
            12
        );
    }

    #[test]
    fn solves_and_aggregates_requested_hero_combos() {
        let result = config().solve(8, 2).unwrap();
        assert_eq!(result.hero.player, 0);
        assert_eq!(result.hero.requested_hands, 2);
        assert!(!result.hero.observed_hands.is_empty());
        assert!(
            (result
                .hero
                .actions
                .iter()
                .map(|action| action.frequency)
                .sum::<f64>()
                - 1.0)
                .abs()
                < 1e-9
        );
    }
}
