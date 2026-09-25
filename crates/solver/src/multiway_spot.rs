//! High-level preflop multiway Hold'em spot adapter.
//!
//! This layer turns a table configuration plus an observed action history into
//! a continuation `GameTree`. It intentionally does not infer post-action
//! ranges from the history: callers must pass ranges appropriate for the
//! selected starting point. The underlying batch solver still owns exact
//! blockers, ChipEV settlement, sampling and convergence diagnostics.

use holdem_cards::{mask_from_cards, Card, DeckMask};
use holdem_domain::setup::build_preflop_state;
use holdem_domain::table::TableConfig;
use holdem_domain::{Action, GameState, PlayerId, PlayerStatus, Street, TerminalState};
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
    /// Публичный борд стартующего спота (T4.1, D-020): пусто — префлоп-старт
    /// (прежнее поведение); 3/4/5 карт — флоп/тёрн/ривер-старт, карты
    /// потребляются границами улиц в `state_after_history`.
    pub board_cards: Vec<Card>,
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
    /// Спека абстракции карт (T3.2, D-019): None — точный режим,
    /// без трансформа дерева (байт-идентичное прежнее поведение).
    pub card_abstraction: Option<crate::card_abstraction::CardAbstractionSpec>,
    /// Число сэмплов дилов для MC-оценки блокировки представителей
    /// (D-019); 0 — оценку не считать (результат без блока blocking).
    pub blocking_samples: usize,
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

/// Version of the result JSON format. The job format keeps
/// `MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION`; results additionally carry
/// `tree_index` from version 2 on, and readers must treat its absence as an
/// older result.
pub const MULTIWAY_HOLDEM_SPOT_RESULT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSpotResult {
    pub tree_fingerprint: u64,
    pub strategy_report: MultiwayBatchStrategyReport,
    pub hero: MultiwayHoldemSpotHeroReport,
    /// One entry per compiled public-tree node, indexed by node id:
    /// `{id, parent, street, board, pot, current_bet, dead_money, actor,
    /// players[], actions[{action, child}], chance[{cards, child}],
    /// terminal}`. Cards use the shared `rank * 4 + suit` encoding and
    /// actions reuse the `ActionExport` shape of strategy reports.
    pub tree_index: Vec<serde_json::Value>,
    /// Отчёт применённой абстракции карт (D-019): None — точный режим.
    pub card_abstraction: Option<crate::card_abstraction::CardAbstractionReport>,
    /// MC-оценка блокировки представителей (D-019): None — не считалась.
    pub blocking_estimate: Option<crate::card_abstraction::BlockingEstimate>,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayHoldemSpotResult {
    schema_version: u32,
    format: &'static str,
    tree_fingerprint: u64,
    strategy_report: serde_json::Value,
    hero: JsonMultiwayHoldemSpotHeroReport,
    tree_index: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    card_abstraction: Option<JsonCardAbstractionOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocking: Option<JsonBlockingEstimate>,
}

#[derive(Debug, Serialize)]
struct JsonBlockingEstimate {
    estimate: bool,
    deals: usize,
    lost_mass: f64,
    lost_fraction: f64,
}

#[derive(Debug, Serialize)]
struct JsonCardAbstractionOutcome {
    mode: String,
    granularity: String,
    equity_groups: Option<usize>,
    flop_fingerprint: Option<u64>,
    turn_fingerprint: Option<u64>,
    river_fingerprint: Option<u64>,
    chance_nodes: usize,
    outcomes_before: u64,
    outcomes_after: u64,
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
            schema_version: MULTIWAY_HOLDEM_SPOT_RESULT_SCHEMA_VERSION,
            format: "multiway_holdem_spot_result",
            tree_fingerprint: self.tree_fingerprint,
            strategy_report,
            hero,
            tree_index: self.tree_index.clone(),
            card_abstraction: self.card_abstraction.as_ref().map(|report| {
                JsonCardAbstractionOutcome {
                    mode: match report.mode {
                        crate::card_abstraction::CardAbstractionMode::Structural => {
                            "structural".to_string()
                        }
                        crate::card_abstraction::CardAbstractionMode::Equity => {
                            "equity".to_string()
                        }
                    },
                    granularity: format!("{}", report.granularity),
                    equity_groups: if report.mode
                        == crate::card_abstraction::CardAbstractionMode::Equity
                    {
                        Some(report.equity_groups)
                    } else {
                        None
                    },
                    flop_fingerprint: report.flop_fingerprint,
                    turn_fingerprint: report.turn_fingerprint,
                    river_fingerprint: report.river_fingerprint,
                    chance_nodes: report.chance_nodes,
                    outcomes_before: report.outcomes_before,
                    outcomes_after: report.outcomes_after,
                }
            }),
            blocking: self
                .blocking_estimate
                .as_ref()
                .map(|estimate| JsonBlockingEstimate {
                    estimate: true,
                    deals: estimate.deals,
                    lost_mass: estimate.lost_mass,
                    lost_fraction: estimate.lost_fraction,
                }),
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
    /// T4.1/D-020: маска карт, гарантированно попадающих на борд спота:
    /// стартовый борд плюс карты, присутствующие во всех явных исходах
    /// каждой достижимой улицы (фиксированные ранауты). Улицы, пройденные
    /// стартовым бордом, недостижимы — их исходы мёртвый конфиг и в маску
    /// не входят. Такие карты не могут оказаться в приватных руках: маска
    /// вливается в dead_cards, сэмплер не генерирует невозможные дилы.
    pub fn certain_public_mask(&self) -> Result<DeckMask, String> {
        let mut mask = mask_from_cards(&self.board_cards).map_err(|error| error.to_string())?;
        if let MultiwayHoldemSpotTreeConfig::Full(config) = &self.tree {
            for (outcomes, min_board_len) in [
                (&config.chance.flop, 3usize),
                (&config.chance.turn, 4),
                (&config.chance.river, 5),
            ] {
                // Улица, пройденная стартовым бордом, недостижима: её
                // явные исходы — мёртвый конфиг (JSON-слой отвергает их
                // на парсинге); в маску входят только достижимые улицы.
                if self.board_cards.len() >= min_board_len {
                    continue;
                }
                if let Some(outcomes) = outcomes {
                    let mut certain_street: Option<DeckMask> = None;
                    for outcome in outcomes {
                        let outcome_mask =
                            mask_from_cards(&outcome.cards).map_err(|error| error.to_string())?;
                        certain_street = Some(match certain_street {
                            None => outcome_mask,
                            Some(previous) => previous & outcome_mask,
                        });
                    }
                    if let Some(street) = certain_street {
                        if street & mask != 0 {
                            return Err("chance outcome cards conflict with earlier public cards"
                                .to_string());
                        }
                        mask |= street;
                    }
                }
            }
        }
        Ok(mask)
    }

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

    /// Порядок постфлоп-действий для реплея границ улиц истории: явный
    /// порядок из Full-конфига, при его отсутствии — порядок стола; тот же
    /// выбор делает `build_tree`, чтобы состояние и дерево не расходились.
    fn effective_postflop_order(&self) -> Vec<PlayerId> {
        let explicit = match &self.tree {
            MultiwayHoldemSpotTreeConfig::Full(config) => config.postflop_order.clone(),
            MultiwayHoldemSpotTreeConfig::Round(_) => Vec::new(),
        };
        if explicit.is_empty() {
            self.table.postflop_order()
        } else {
            explicit
        }
    }

    pub fn state_after_history(&self) -> Result<GameState, String> {
        self.validate()?;
        let mut state = build_preflop_state(&self.table)?;
        let mut board_index = 0usize;
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
            // T4.1/D-020: закрытый раунд при недоигранном борде — граница улицы.
            if state.terminal.is_none() && state.actor.is_none() {
                let remaining = self.board_cards.len() - board_index;
                if remaining == 0 {
                    return Err(
                        "spot history ends between betting rounds without an actor".to_string()
                    );
                }
                let needed = state.street.required_new_board_cards();
                if needed == 0 || remaining < needed {
                    return Err(format!(
                        "spot board has {remaining} card(s) left, but street needs {needed}"
                    ));
                }
                let new_cards = &self.board_cards[board_index..board_index + needed];
                state.advance_to_next_street(new_cards, &self.effective_postflop_order())?;
                board_index += needed;
            }
        }
        if state.terminal.is_some() {
            return Err("spot history ends at a terminal state, not a hero decision".to_string());
        }
        if state.actor.is_none() {
            return Err("spot history ends between betting rounds without an actor".to_string());
        }
        if board_index < self.board_cards.len() {
            return Err(format!(
                "spot board has {} unconsumed card(s) while a decision is pending",
                self.board_cards.len() - board_index
            ));
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
            self.dead_cards | self.certain_public_mask()?,
            self.seed,
            config_fingerprint,
            self.max_private_attempts,
        )?;
        Ok((tree, solver))
    }

    /// build_solver с применением card_abstraction (T3.2, D-019).
    /// Возвращает дополнительно отчёт абстракции, когда она включена.
    pub fn build_solver_with_abstraction(
        &self,
    ) -> Result<
        (
            GameTree,
            MultiwayHoldemBatchSolver,
            Option<(
                crate::card_abstraction::CardAbstractionReport,
                Option<crate::card_abstraction::BlockingEstimate>,
            )>,
        ),
        String,
    > {
        let tree = self.build_tree()?;
        match &self.card_abstraction {
            None => {
                let solver = self.build_solver_from_tree(tree.clone())?;
                Ok((tree, solver, None))
            }
            Some(spec) => {
                let (tree, report, groupings) =
                    crate::card_abstraction::apply_card_abstraction_detailed(tree, spec)?;
                let blocking = if self.blocking_samples > 0 {
                    let range_refs: Vec<&WeightedRange> = self.ranges.iter().collect();
                    Some(crate::card_abstraction::estimate_representative_blocking(
                        &tree,
                        &groupings,
                        &range_refs,
                        self.dead_cards | self.certain_public_mask()?,
                        self.seed,
                        self.blocking_samples,
                    )?)
                } else {
                    None
                };
                let solver = self.build_solver_from_tree(tree.clone())?;
                Ok((tree, solver, Some((report, blocking))))
            }
        }
    }

    fn build_solver_from_tree(&self, tree: GameTree) -> Result<MultiwayHoldemBatchSolver, String> {
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
        MultiwayHoldemBatchSolver::new(
            tree.clone(),
            self.ranges.clone(),
            self.dead_cards | self.certain_public_mask()?,
            self.seed,
            config_fingerprint,
            self.max_private_attempts,
        )
    }

    /// Собирает результат с отчётом применённой абстракции карт (D-019):
    /// None — точный режим, отчёта нет.
    pub fn result_from_solver_with_abstraction(
        &self,
        tree: &GameTree,
        solver: &MultiwayHoldemBatchSolver,
        utility_samples: usize,
        abstraction_report: Option<crate::card_abstraction::CardAbstractionReport>,
        blocking_estimate: Option<crate::card_abstraction::BlockingEstimate>,
    ) -> Result<MultiwayHoldemSpotResult, String> {
        let mut result = self.result_from_solver(tree, solver, utility_samples)?;
        result.card_abstraction = abstraction_report;
        result.blocking_estimate = blocking_estimate;
        Ok(result)
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
            tree_index: tree_index_json(tree),
            card_abstraction: None,
            blocking_estimate: None,
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
        // T3.2/D-019: абстракция карт применяется к публичному дереву до
        // передачи в солвер — компиляторы/чекпойнты/отчёты работают с
        // трансформированным деревом без правок.
        let (tree, mut solver, abstraction) = self.build_solver_with_abstraction()?;
        let (abstraction_report, blocking_estimate) = match abstraction {
            Some((report, blocking)) => (Some(report), blocking),
            None => (None, None),
        };
        solver.run_parallel(iterations, self.worker_count, self.reduction_batch_size)?;
        self.result_from_solver_with_abstraction(
            &tree,
            &solver,
            utility_samples,
            abstraction_report,
            blocking_estimate,
        )
    }
}

fn tree_index_json(tree: &GameTree) -> Vec<serde_json::Value> {
    tree.nodes
        .iter()
        .map(|node| {
            let state = &node.state;
            let street = match state.street {
                Street::Preflop => "preflop",
                Street::Flop => "flop",
                Street::Turn => "turn",
                Street::River => "river",
            };
            let actions = node
                .children
                .iter()
                .filter_map(|&child| {
                    tree.nodes
                        .get(child)?
                        .action_from_parent
                        .as_ref()
                        .map(|action| {
                            serde_json::json!({
                                "action": crate::ActionExport::from_action(action),
                                "child": child,
                            })
                        })
                })
                .collect::<Vec<_>>();
            let chance = node.chance.as_ref().map(|chance| {
                chance
                    .outcomes
                    .iter()
                    .zip(node.children.iter())
                    .map(|(outcome, &child)| {
                        serde_json::json!({ "cards": outcome.cards, "child": child })
                    })
                    .collect::<Vec<_>>()
            });
            let terminal = node.leaf.as_ref().map(|leaf| match leaf {
                LeafKind::RoundComplete => serde_json::json!("round_complete"),
                LeafKind::Terminal(TerminalState::Fold { winner }) => {
                    serde_json::json!({ "kind": "fold", "winner": winner })
                }
                LeafKind::Terminal(TerminalState::Showdown) => {
                    serde_json::json!({ "kind": "showdown" })
                }
            });
            serde_json::json!({
                "id": node.id,
                "parent": node.parent,
                "street": street,
                "board": state.board,
                "pot": state.pot,
                "current_bet": state.current_bet,
                "dead_money": state.dead_money,
                "actor": state.actor,
                "players": state
                    .players
                    .iter()
                    .map(|player| {
                        serde_json::json!({
                            "seat": player.seat,
                            "stack": player.stack_remaining,
                            "committed": player.committed_total,
                            "status": match player.status {
                                PlayerStatus::Active => "active",
                                PlayerStatus::Folded => "folded",
                                PlayerStatus::AllIn => "all_in",
                                PlayerStatus::OutOfHand => "out_of_hand",
                            },
                        })
                    })
                    .collect::<Vec<_>>(),
                "actions": actions,
                "chance": chance,
                "terminal": terminal,
            })
        })
        .collect()
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
            board_cards: Vec::new(),
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
            card_abstraction: None,
            blocking_samples: 0,
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

    #[test]
    fn spot_result_exposes_consistent_tree_index() {
        let config = config();
        let tree = config.build_tree().unwrap();
        let result = config.solve(2, 1).unwrap();
        assert_eq!(result.tree_index.len(), tree.nodes.len());
        for (node, entry) in tree.nodes.iter().zip(result.tree_index.iter()) {
            assert_eq!(entry["id"], serde_json::json!(node.id));
            let mut listed: Vec<u64> = entry["actions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|edge| edge["child"].as_u64().unwrap())
                .collect();
            if let Some(chance) = entry["chance"].as_array() {
                listed.extend(chance.iter().map(|edge| edge["child"].as_u64().unwrap()));
            }
            listed.sort_unstable();
            let mut expected: Vec<u64> = node.children.iter().map(|&child| child as u64).collect();
            expected.sort_unstable();
            assert_eq!(listed, expected, "children mismatch at node {}", node.id);
        }
        let hero_entry = &result.tree_index[result.hero.public_node];
        assert_eq!(hero_entry["actor"], serde_json::json!(config.hero_player));
        assert!(!hero_entry["actions"].as_array().unwrap().is_empty());
        let value: serde_json::Value = serde_json::from_str(&result.to_json().unwrap()).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(crate::multiway_spot::MULTIWAY_HOLDEM_SPOT_RESULT_SCHEMA_VERSION)
        );
        assert_eq!(
            value["tree_index"].as_array().unwrap().len(),
            tree.nodes.len()
        );
    }

    #[test]
    fn spot_card_abstraction_wiring_structural_report_and_blocking() {
        use crate::card_abstraction::{CardAbstractionMode, CardAbstractionSpec};
        use holdem_cards::cards_from_str;
        use holdem_domain::ActionSizes;
        use holdem_ranges::parse_range;
        use holdem_solver_abstraction::Granularity;

        // Трёхместный стол: минимальный валидный multiway-спот.
        let table = TableConfig {
            table_size: 3,
            button: 0,
            small_blind: 500,
            big_blind: 1_000,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![10_000; 3],
            dead_money: 0,
        };
        // Уникальные узкие диапазоны, чтобы сэмплер гарантированно
        // находил легальные дилы, и карты не пересекались с бордом.
        let ranges: Vec<WeightedRange> = ["AKs", "KQs", "QQ"]
            .iter()
            .map(|text| WeightedRange::from_classes(&parse_range(text).unwrap()))
            .collect();
        let mut config = MultiwayHoldemSpotConfig {
            board_cards: Vec::new(),
            table,
            action_history: vec![],
            ranges,
            dead_cards: 0,
            tree: MultiwayHoldemSpotTreeConfig::Full(FullTreeBuildConfig {
                round: TreeBuildConfig {
                    action_sizes: ActionSizes {
                        bet_to: vec![],
                        raise_to: vec![2_500],
                        include_all_in: false,
                    },
                    abstraction: None,
                    max_nodes: 50_000,
                    max_depth: 32,
                },
                chance: ChanceConfig {
                    flop: Some(vec![
                        ChanceOutcome::new(cards_from_str("As Ks Qs").unwrap(), 1.0),
                        ChanceOutcome::new(cards_from_str("Ah Kh Qh").unwrap(), 1.0),
                        ChanceOutcome::new(cards_from_str("2d 7d 9c").unwrap(), 1.0),
                    ]),
                    turn: Some(vec![ChanceOutcome::new(cards_from_str("Jh").unwrap(), 1.0)]),
                    river: Some(vec![ChanceOutcome::new(cards_from_str("3s").unwrap(), 1.0)]),
                    enumerate_exact: false,
                    max_outcomes_per_node: 10_000,
                },
                postflop_order: vec![1, 2, 0],
            }),
            hero_player: 0,
            hero_hands: combos_for_hand("AKs").unwrap(),
            hero_label: "AKs".to_string(),
            seed: 7,
            config_fingerprint: 0,
            max_private_attempts: 100,
            worker_count: 1,
            reduction_batch_size: 1,
            card_abstraction: None,
            blocking_samples: 32,
        };

        // Точный режим: решаем базовый спот без абстракции.
        let exact = config.clone().solve(4, 1).unwrap();
        assert!(exact.card_abstraction.is_none());
        assert!(exact.blocking_estimate.is_none());

        // Structural: отчёт абстракции + MC-оценка блокировки в проводке.
        config.card_abstraction = Some(CardAbstractionSpec::structural(Granularity::Coarse));
        let abstracted = config.clone().solve(4, 1).unwrap();
        let report = abstracted.card_abstraction.as_ref().unwrap();
        assert!(matches!(report.mode, CardAbstractionMode::Structural));
        assert!(report.chance_nodes > 0);
        // 3 флоп-исхода -> 2 бакета (монотонные близнецы сливаются).
        assert!(report.outcomes_before > report.outcomes_after);
        assert!(report.flop_fingerprint.is_some());
        assert!(report.turn_fingerprint.is_none());
        assert!(report.river_fingerprint.is_none());

        // Оценка блокировки: посчитана, в границах, детерминирована.
        let estimate = abstracted.blocking_estimate.as_ref().unwrap();
        assert!(estimate.deals > 0);
        assert!((0.0..=1.0).contains(&estimate.lost_fraction));
        let rerun = config.solve(4, 1).unwrap();
        let rerun_estimate = rerun.blocking_estimate.as_ref().unwrap();
        assert_eq!(estimate.deals, rerun_estimate.deals);
        assert!((estimate.lost_fraction - rerun_estimate.lost_fraction).abs() < 1e-12);

        // JSON: блоки card_abstraction и blocking присутствуют.
        let json = abstracted.to_json().unwrap();
        assert!(json.contains("\"card_abstraction\""));
        assert!(json.contains("\"blocking\""));
        assert!(json.contains("\"lost_fraction\""));

        // blocking_samples = 0: оценки нет, блока blocking в JSON нет.
        let mut quiet = config.clone();
        quiet.blocking_samples = 0;
        let quiet_result = quiet.solve(4, 1).unwrap();
        assert!(quiet_result.blocking_estimate.is_none());
        let quiet_json = quiet_result.to_json().unwrap();
        assert!(!quiet_json.contains("\"blocking\""));
    }

    #[test]
    fn postflop_start_replays_srp_history_to_flop() {
        let mut config = config();
        config.action_history = vec![
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Raise { to: 5 },
            },
            MultiwayHoldemSpotAction {
                player: 1,
                action: Action::Fold,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Call,
            },
        ];
        config.board_cards = holdem_cards::cards_from_str("2s 3d 4c").unwrap();
        config.hero_player = 2;
        config.ranges[2] = range(&["Qs Qh"]);
        config.hero_hands = vec![combo("Qs Qh")];
        config.hero_label = "QQ".to_string();
        let state = config.state_after_history().unwrap();
        assert_eq!(state.street, Street::Flop);
        assert_eq!(state.actor, Some(2));
        assert_eq!(
            state.board,
            holdem_cards::cards_from_str("2s 3d 4c").unwrap()
        );
        assert_eq!(state.current_bet, 0);
        assert_eq!(state.pot, 11);
        // Full-дерево строится из флоп-старта: корень — узел решений.
        let tree = config.build_tree().unwrap();
        assert!(tree.nodes[0].chance.is_none());
        assert!(tree.nodes[0].children.len() >= 2);
    }

    #[test]
    fn postflop_start_round_mode_builds_round_tree() {
        let mut config = config();
        config.action_history = vec![
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Raise { to: 5 },
            },
            MultiwayHoldemSpotAction {
                player: 1,
                action: Action::Fold,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Call,
            },
        ];
        config.board_cards = holdem_cards::cards_from_str("2s 3d 4c").unwrap();
        config.hero_player = 2;
        config.ranges[2] = range(&["Qs Qh"]);
        config.hero_hands = vec![combo("Qs Qh")];
        config.tree = MultiwayHoldemSpotTreeConfig::Round(TreeBuildConfig {
            action_sizes: holdem_domain::ActionSizes {
                bet_to: vec![3],
                raise_to: Vec::new(),
                include_all_in: false,
            },
            abstraction: None,
            max_nodes: 10_000,
            max_depth: 32,
        });
        let tree = config.build_tree().unwrap();
        let round_complete = tree
            .leaf_nodes()
            .any(|node| matches!(node.leaf.as_ref(), Some(LeafKind::RoundComplete)));
        assert!(round_complete);
    }

    #[test]
    fn postflop_start_consumes_street_boundaries_to_river() {
        let mut config = config();
        config.action_history = vec![
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Raise { to: 5 },
            },
            MultiwayHoldemSpotAction {
                player: 1,
                action: Action::Fold,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Call,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Check,
            },
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Check,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Check,
            },
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Check,
            },
        ];
        config.board_cards = holdem_cards::cards_from_str("2s 3d 4c 5h 6s").unwrap();
        config.hero_player = 2;
        config.ranges[2] = range(&["Qs Qh"]);
        config.hero_hands = vec![combo("Qs Qh")];
        let state = config.state_after_history().unwrap();
        assert_eq!(state.street, Street::River);
        assert_eq!(state.board.len(), 5);
        assert_eq!(state.actor, Some(2));
    }

    #[test]
    fn postflop_start_rejects_inconsistent_boards() {
        // Борд длиннее закрытых улиц: карта осталась неиспользованной.
        let mut longer = config();
        longer.action_history = vec![
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Raise { to: 5 },
            },
            MultiwayHoldemSpotAction {
                player: 1,
                action: Action::Fold,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Call,
            },
        ];
        longer.board_cards = holdem_cards::cards_from_str("2s 3d 4c 5h").unwrap();
        longer.hero_player = 2;
        longer.ranges[2] = range(&["Qs Qh"]);
        longer.hero_hands = vec![combo("Qs Qh")];
        let error = longer.state_after_history().unwrap_err();
        assert!(error.contains("unconsumed"));

        // Длина борда вне {0, 3, 4, 5}: борд не расходуется историей.
        let mut two_cards = config();
        two_cards.board_cards = holdem_cards::cards_from_str("2s 7d").unwrap();
        let error = two_cards.state_after_history().unwrap_err();
        assert!(error.contains("unconsumed"));

        // Прежняя ошибка сохранена: закрытый раунд без борда.
        let mut closed = config();
        closed.action_history = vec![
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Raise { to: 5 },
            },
            MultiwayHoldemSpotAction {
                player: 1,
                action: Action::Fold,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Call,
            },
        ];
        let error = closed.state_after_history().unwrap_err();
        assert!(error.contains("between betting rounds"));
        assert!(error.contains("between betting rounds"));
    }

    #[test]
    fn postflop_start_solve_with_fixed_runout_and_wide_range() {
        // Регрессия инцидента solve-смока: фиксированный ранаут (5h/6s из
        // конфига) + широкий SB с картами ранаута. До фикса сэмплер
        // генерировал невозможные дилы ("no legal public outcomes");
        // certain-маска исключает их на этапе сэмплирования.
        let mut config = config();
        config.action_history = vec![
            MultiwayHoldemSpotAction {
                player: 0,
                action: Action::Raise { to: 5 },
            },
            MultiwayHoldemSpotAction {
                player: 1,
                action: Action::Fold,
            },
            MultiwayHoldemSpotAction {
                player: 2,
                action: Action::Call,
            },
        ];
        config.board_cards = holdem_cards::cards_from_str("2s 3d 4c").unwrap();
        config.hero_player = 2;
        config.ranges[1] = range(&["5h 4h", "3h 2h"]);
        config.ranges[2] = range(&["Qs Qh"]);
        config.hero_hands = vec![combo("Qs Qh")];
        let result = config.solve(8, 1).unwrap();
        assert_ne!(result.tree_fingerprint, 0);
        assert!(!result.hero.observed_hands.is_empty());
    }
}
