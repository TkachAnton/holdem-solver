//! Adapter from the public-information Hold'em tree to `StaticGame`.
//!
//! The adapter deliberately compiles a perfect-information tree: every public
//! decision node receives its own information-set id. Private-card-conditioned
//! information sets are a separate layer and must not be approximated by this
//! mapping.

use std::collections::{BTreeMap, HashMap, HashSet};

use holdem_cards::{mask_from_cards, DeckMask};
use holdem_domain::{Action, Chips, PlayerId, TerminalState};
use holdem_ranges::{combos_for_hand, rank_char, Combo, HandClassId, WeightedRange};
use holdem_settlement::exact_chip_ev_showdown;
use holdem_tree::{GameTree, LeafKind, NodeId as TreeNodeId, TreeNode};
use serde::Serialize;

use crate::{
    CfrPlusSolver, GameNode, InfoSetCheckpoint, InfoSetId, MccfrSolver, NodeId, SolverAlgorithm,
    SolverCheckpoint, StaticGame,
};

/// Supplies utilities for terminal nodes while compiling a Hold'em tree.
pub trait TerminalPayoff {
    fn utility(&self, node: &TreeNode) -> Result<[f64; 2], String>;
}

/// A compiled tree plus the action labels that belong to each solver node.
///
/// `StaticGame` intentionally stores only child ids. This wrapper keeps the
/// original Hold'em actions alongside it so callers can map strategy vector
/// positions back to `Fold`, `Call`, `Raise`, and so on.
#[derive(Debug, Clone)]
pub struct CompiledHoldemGame {
    game: StaticGame,
    actions: Vec<Vec<Action>>,
    tree_node_ids: Vec<Option<TreeNodeId>>,
    private_hands: Vec<Option<[Combo; 2]>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrivateDeal {
    pub probability: f64,
    pub hands: [Combo; 2],
}

/// Expands two weighted exact-combo ranges into blocker-aware private deals.
///
/// Input weights need not be normalized. The product of the two combo weights
/// is conditioned on both hands being legal together and on `dead_cards`.
/// Duplicate range entries are aggregated deterministically by exact hand pair.
pub fn private_deals_from_ranges(
    ranges: [&WeightedRange; 2],
    dead_cards: DeckMask,
) -> Result<Vec<PrivateDeal>, String> {
    for (range_index, range) in ranges.iter().enumerate() {
        for entry in &range.combos {
            if !entry.weight.is_finite() || entry.weight < 0.0 {
                return Err(format!(
                    "range {range_index} contains an invalid combo weight"
                ));
            }
        }
    }

    let mut weighted_deals = BTreeMap::<[Combo; 2], f64>::new();
    for first in &ranges[0].combos {
        if first.weight == 0.0 || first.combo.conflicts(dead_cards) {
            continue;
        }
        for second in &ranges[1].combos {
            if !second.weight.is_finite() || second.weight < 0.0 {
                return Err("second range contains an invalid combo weight".to_string());
            }
            if second.weight == 0.0
                || second.combo.conflicts(dead_cards)
                || second.combo.conflicts(first.combo.mask())
            {
                continue;
            }
            let weight = first.weight * second.weight;
            if !weight.is_finite() || weight < 0.0 {
                return Err("range product has an invalid weight".to_string());
            }
            *weighted_deals
                .entry([first.combo, second.combo])
                .or_insert(0.0) += weight;
        }
    }

    let total: f64 = weighted_deals.values().sum();
    if total <= 0.0 || !total.is_finite() {
        return Err("ranges have no legal private-card deals".to_string());
    }
    Ok(weighted_deals
        .into_iter()
        .map(|(hands, weight)| PrivateDeal {
            probability: weight / total,
            hands,
        })
        .collect())
}

impl CompiledHoldemGame {
    pub fn game(&self) -> &StaticGame {
        &self.game
    }

    pub fn into_game(self) -> StaticGame {
        self.game
    }

    pub fn actions_at(&self, node_id: NodeId) -> Option<&[Action]> {
        self.actions.get(node_id).map(Vec::as_slice)
    }

    pub fn action(&self, node_id: NodeId, action_index: usize) -> Option<&Action> {
        self.actions_at(node_id)?.get(action_index)
    }

    pub fn tree_node_id(&self, solver_node_id: NodeId) -> Option<TreeNodeId> {
        self.tree_node_ids.get(solver_node_id).copied().flatten()
    }

    pub fn private_hands_at(&self, solver_node_id: NodeId) -> Option<&[Combo; 2]> {
        self.private_hands.get(solver_node_id)?.as_ref()
    }

    pub fn node_count(&self) -> usize {
        self.game.nodes().len()
    }

    pub fn private_deal_count(&self) -> usize {
        match self.game.node(self.game.root()) {
            Some(crate::GameNode::Chance { outcomes }) => outcomes.len(),
            _ => 0,
        }
    }
}

/// Result of one finite-deal Hold'em solver batch.
#[derive(Debug, Clone)]
pub struct HoldemSolveResult {
    pub checkpoint: SolverCheckpoint,
    pub average_utility: [f64; 2],
    pub game_nodes: usize,
    pub private_deals: usize,
}

#[derive(Debug, Clone)]
pub struct ActionReport {
    pub action: Action,
    pub frequency: f64,
    pub counterfactual_ev: Option<f64>,
    pub ev_loss: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct InfoSetReport {
    pub player: PlayerId,
    pub infoset: InfoSetId,
    pub public_node: Option<TreeNodeId>,
    pub private_hand: Option<Combo>,
    pub occurrences: usize,
    pub visits: u64,
    pub actions: Vec<ActionReport>,
}

#[derive(Debug, Clone)]
pub struct StrategyReport {
    pub algorithm: SolverAlgorithm,
    pub iterations: u64,
    pub average_utility: [f64; 2],
    pub infosets: Vec<InfoSetReport>,
}

#[derive(Debug, Clone)]
pub struct RangeActionReport {
    pub action: Action,
    pub frequency: f64,
    pub counterfactual_ev: Option<f64>,
    pub ev_loss: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct RangeClassReport {
    pub player: PlayerId,
    pub public_node: Option<TreeNodeId>,
    pub class_id: HandClassId,
    pub combo_count: usize,
    pub marginal_weight: f64,
    pub actions: Vec<RangeActionReport>,
}

#[derive(Debug, Clone)]
pub struct RangeStrategyReport {
    pub algorithm: SolverAlgorithm,
    pub iterations: u64,
    pub average_utility: [f64; 2],
    pub classes: Vec<RangeClassReport>,
}

/// Version of the machine-readable range result schema.
pub const RANGE_RESULT_SCHEMA_VERSION: u32 = 1;
/// Number of rows and columns in the conventional Hold'em 13x13 matrix.
pub const RANGE_MATRIX_SIZE: usize = 13;

/// A conventional preflop 13x13 matrix cell projected from a class report.
///
/// Rows and columns are ordered `A, K, Q, ..., 2`. The upper triangle is
/// suited (`AKs`), the lower triangle is offsuit (`AKo`), and the diagonal is
/// pairs (`AA`). A cell can be absent when the finite deal/report did not
/// contain that class; the cell remains in the matrix so consumers always get
/// a rectangular 169-cell grid.
#[derive(Debug, Clone)]
pub struct RangeMatrixCell {
    pub row: usize,
    pub column: usize,
    pub row_rank: char,
    pub column_rank: char,
    pub class_name: String,
    pub class_id: HandClassId,
    pub combo_count: Option<usize>,
    pub marginal_weight: Option<f64>,
    pub actions: Vec<RangeActionReport>,
}

#[derive(Debug, Clone)]
pub struct RangeMatrix {
    pub player: PlayerId,
    pub public_node: Option<TreeNodeId>,
    pub cells: Vec<RangeMatrixCell>,
}

#[derive(Debug, Clone)]
pub struct RangeMatrixReport {
    pub schema_version: u32,
    pub algorithm: SolverAlgorithm,
    pub iterations: u64,
    pub average_utility: [f64; 2],
    pub rank_order: Vec<char>,
    pub matrices: Vec<RangeMatrix>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionExport {
    Fold,
    Check,
    Call,
    Bet { to: Chips },
    Raise { to: Chips },
    AllIn,
}

impl ActionExport {
    pub fn from_action(action: &Action) -> Self {
        match action {
            Action::Fold => Self::Fold,
            Action::Check => Self::Check,
            Action::Call => Self::Call,
            Action::Bet { to } => Self::Bet { to: *to },
            Action::Raise { to } => Self::Raise { to: *to },
            Action::AllIn => Self::AllIn,
        }
    }
}

impl RangeStrategyReport {
    /// Projects the exact class report into one complete 13x13 matrix for
    /// every `(player, public_node)` context present in the report.
    pub fn to_matrix(&self) -> Result<RangeMatrixReport, String> {
        let canonical = canonical_matrix_classes()?;
        let mut by_context = BTreeMap::<
            (PlayerId, Option<TreeNodeId>),
            BTreeMap<HandClassId, &RangeClassReport>,
        >::new();

        for class in &self.classes {
            if !canonical.iter().any(|(_, id, _)| *id == class.class_id) {
                return Err(format!(
                    "class id {} is not a canonical 13x13 Hold'em class",
                    class.class_id
                ));
            }
            let context = by_context
                .entry((class.player, class.public_node))
                .or_default();
            if context.insert(class.class_id, class).is_some() {
                return Err(format!(
                    "duplicate class {} at player {} node {:?}",
                    class.class_id, class.player, class.public_node
                ));
            }
        }

        let matrices = by_context
            .into_iter()
            .map(|((player, public_node), reports)| {
                let cells: Vec<RangeMatrixCell> = canonical
                    .iter()
                    .map(|(class_name, class_id, row_column)| {
                        let report = reports.get(class_id).copied();
                        let (row, column) = *row_column;
                        RangeMatrixCell {
                            row,
                            column,
                            row_rank: descending_rank(row),
                            column_rank: descending_rank(column),
                            class_name: class_name.clone(),
                            class_id: *class_id,
                            combo_count: report.map(|value| value.combo_count),
                            marginal_weight: report.map(|value| value.marginal_weight),
                            actions: report
                                .map(|value| {
                                    value
                                        .actions
                                        .iter()
                                        .map(|action| RangeActionReport {
                                            action: action.action.clone(),
                                            frequency: action.frequency,
                                            counterfactual_ev: action.counterfactual_ev,
                                            ev_loss: action.ev_loss,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                        }
                    })
                    .collect();
                debug_assert_eq!(cells.len(), RANGE_MATRIX_SIZE * RANGE_MATRIX_SIZE);
                RangeMatrix {
                    player,
                    public_node,
                    cells,
                }
            })
            .collect();

        Ok(RangeMatrixReport {
            schema_version: RANGE_RESULT_SCHEMA_VERSION,
            algorithm: self.algorithm,
            iterations: self.iterations,
            average_utility: self.average_utility,
            rank_order: (0..RANGE_MATRIX_SIZE).map(descending_rank).collect(),
            matrices,
        })
    }

    /// Stable JSON export containing both class-level data and complete 13x13
    /// matrix projections. Action labels are structured objects rather than
    /// Rust debug strings.
    pub fn to_json(&self) -> Result<String, String> {
        let matrix = self.to_matrix()?;
        JsonRangeStrategyReport::from_reports(self, &matrix).to_json()
    }

    /// Long-form CSV export: one row per matrix cell and action. Empty classes
    /// still produce one row, preserving the complete 169-cell grid.
    pub fn to_csv(&self) -> Result<String, String> {
        self.to_matrix()?.to_csv()
    }
}

impl RangeMatrixReport {
    pub fn to_json(&self) -> Result<String, String> {
        JsonRangeMatrixReport::from_report(self).to_json()
    }

    pub fn to_csv(&self) -> Result<String, String> {
        matrix_report_to_csv(self)
    }
}

fn algorithm_name(algorithm: SolverAlgorithm) -> &'static str {
    match algorithm {
        SolverAlgorithm::CfrPlus => "cfr_plus",
        SolverAlgorithm::ExternalSamplingMccfr => "external_sampling_mccfr",
    }
}

fn descending_rank(index: usize) -> char {
    rank_char(RANGE_MATRIX_SIZE - 1 - index).expect("13x13 matrix rank index is valid")
}

fn canonical_matrix_classes() -> Result<Vec<(String, HandClassId, (usize, usize))>, String> {
    let mut classes = Vec::with_capacity(RANGE_MATRIX_SIZE * RANGE_MATRIX_SIZE);
    for row in 0..RANGE_MATRIX_SIZE {
        for column in 0..RANGE_MATRIX_SIZE {
            let row_rank = descending_rank(row);
            let column_rank = descending_rank(column);
            let name = if row == column {
                format!("{row_rank}{column_rank}")
            } else if row < column {
                format!("{row_rank}{column_rank}s")
            } else {
                format!("{column_rank}{row_rank}o")
            };
            let combos = combos_for_hand(&name)?;
            let class_id = combos
                .first()
                .copied()
                .map(Combo::class_id)
                .ok_or_else(|| format!("canonical class has no combos: {name}"))?;
            classes.push((name, class_id, (row, column)));
        }
    }
    Ok(classes)
}

#[derive(Debug, Serialize)]
struct JsonRangeStrategyReport {
    schema_version: u32,
    format: &'static str,
    algorithm: &'static str,
    iterations: u64,
    average_utility: [f64; 2],
    rank_order: Vec<char>,
    classes: Vec<JsonRangeClassReport>,
    matrices: Vec<JsonRangeMatrix>,
}

impl JsonRangeStrategyReport {
    fn from_reports(strategy: &RangeStrategyReport, matrix: &RangeMatrixReport) -> Self {
        Self {
            schema_version: matrix.schema_version,
            format: "range_strategy_report",
            algorithm: algorithm_name(strategy.algorithm),
            iterations: strategy.iterations,
            average_utility: strategy.average_utility,
            rank_order: matrix.rank_order.clone(),
            classes: strategy
                .classes
                .iter()
                .map(JsonRangeClassReport::from_report)
                .collect(),
            matrices: matrix
                .matrices
                .iter()
                .map(JsonRangeMatrix::from_report)
                .collect(),
        }
    }

    fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Serialize)]
struct JsonRangeMatrixReport {
    schema_version: u32,
    format: &'static str,
    algorithm: &'static str,
    iterations: u64,
    average_utility: [f64; 2],
    rank_order: Vec<char>,
    matrices: Vec<JsonRangeMatrix>,
}

impl JsonRangeMatrixReport {
    fn from_report(report: &RangeMatrixReport) -> Self {
        Self {
            schema_version: report.schema_version,
            format: "range_matrix_report",
            algorithm: algorithm_name(report.algorithm),
            iterations: report.iterations,
            average_utility: report.average_utility,
            rank_order: report.rank_order.clone(),
            matrices: report
                .matrices
                .iter()
                .map(JsonRangeMatrix::from_report)
                .collect(),
        }
    }

    fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Serialize)]
struct JsonRangeMatrix {
    player: PlayerId,
    public_node: Option<TreeNodeId>,
    cells: Vec<JsonRangeMatrixCell>,
}

impl JsonRangeMatrix {
    fn from_report(report: &RangeMatrix) -> Self {
        Self {
            player: report.player,
            public_node: report.public_node,
            cells: report
                .cells
                .iter()
                .map(JsonRangeMatrixCell::from_report)
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonRangeMatrixCell {
    row: usize,
    column: usize,
    row_rank: char,
    column_rank: char,
    class_name: String,
    class_id: HandClassId,
    combo_count: Option<usize>,
    marginal_weight: Option<f64>,
    actions: Vec<JsonRangeActionReport>,
}

impl JsonRangeMatrixCell {
    fn from_report(report: &RangeMatrixCell) -> Self {
        Self {
            row: report.row,
            column: report.column,
            row_rank: report.row_rank,
            column_rank: report.column_rank,
            class_name: report.class_name.clone(),
            class_id: report.class_id,
            combo_count: report.combo_count,
            marginal_weight: report.marginal_weight,
            actions: report
                .actions
                .iter()
                .map(JsonRangeActionReport::from_report)
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonRangeClassReport {
    player: PlayerId,
    public_node: Option<TreeNodeId>,
    class_id: HandClassId,
    combo_count: usize,
    marginal_weight: f64,
    actions: Vec<JsonRangeActionReport>,
}

impl JsonRangeClassReport {
    fn from_report(report: &RangeClassReport) -> Self {
        Self {
            player: report.player,
            public_node: report.public_node,
            class_id: report.class_id,
            combo_count: report.combo_count,
            marginal_weight: report.marginal_weight,
            actions: report
                .actions
                .iter()
                .map(JsonRangeActionReport::from_report)
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonRangeActionReport {
    action: ActionExport,
    frequency: f64,
    counterfactual_ev: Option<f64>,
    ev_loss: Option<f64>,
}

impl JsonRangeActionReport {
    fn from_report(report: &RangeActionReport) -> Self {
        Self {
            action: ActionExport::from_action(&report.action),
            frequency: report.frequency,
            counterfactual_ev: report.counterfactual_ev,
            ev_loss: report.ev_loss,
        }
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn csv_row(fields: &[String]) -> String {
    fields
        .iter()
        .map(|field| csv_escape(field))
        .collect::<Vec<_>>()
        .join(",")
}

fn optional_csv<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn action_csv_fields(action: Option<&RangeActionReport>) -> [String; 5] {
    let Some(action) = action else {
        return [
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ];
    };
    let (kind, target) = match &action.action {
        Action::Fold => ("fold", None),
        Action::Check => ("check", None),
        Action::Call => ("call", None),
        Action::Bet { to } => ("bet", Some(*to)),
        Action::Raise { to } => ("raise", Some(*to)),
        Action::AllIn => ("all_in", None),
    };
    [
        kind.to_string(),
        optional_csv(target),
        action.frequency.to_string(),
        optional_csv(action.counterfactual_ev),
        optional_csv(action.ev_loss),
    ]
}

fn matrix_report_to_csv(report: &RangeMatrixReport) -> Result<String, String> {
    let mut lines = vec![csv_row(&[
        "schema_version".to_string(),
        "format".to_string(),
        "algorithm".to_string(),
        "iterations".to_string(),
        "average_utility_player_0".to_string(),
        "average_utility_player_1".to_string(),
        "player".to_string(),
        "public_node".to_string(),
        "row".to_string(),
        "column".to_string(),
        "row_rank".to_string(),
        "column_rank".to_string(),
        "class_name".to_string(),
        "class_id".to_string(),
        "combo_count".to_string(),
        "marginal_weight".to_string(),
        "action_kind".to_string(),
        "action_to".to_string(),
        "frequency".to_string(),
        "counterfactual_ev".to_string(),
        "ev_loss".to_string(),
    ])];

    for matrix in &report.matrices {
        for cell in &matrix.cells {
            let action_rows: Vec<Option<&RangeActionReport>> = if cell.actions.is_empty() {
                vec![None]
            } else {
                cell.actions.iter().map(Some).collect()
            };
            for action in action_rows {
                let action_fields = action_csv_fields(action);
                lines.push(csv_row(&[
                    report.schema_version.to_string(),
                    "range_matrix".to_string(),
                    algorithm_name(report.algorithm).to_string(),
                    report.iterations.to_string(),
                    report.average_utility[0].to_string(),
                    report.average_utility[1].to_string(),
                    matrix.player.to_string(),
                    optional_csv(matrix.public_node),
                    cell.row.to_string(),
                    cell.column.to_string(),
                    cell.row_rank.to_string(),
                    cell.column_rank.to_string(),
                    cell.class_name.clone(),
                    cell.class_id.to_string(),
                    optional_csv(cell.combo_count),
                    optional_csv(cell.marginal_weight),
                    action_fields[0].clone(),
                    action_fields[1].clone(),
                    action_fields[2].clone(),
                    action_fields[3].clone(),
                    action_fields[4].clone(),
                ]));
            }
        }
    }

    Ok(format!("{}\n", lines.join("\n")))
}

/// Aggregates an exact-combo strategy report into class/range buckets.
///
/// Marginal weights are derived from the supplied blocker-conditioned private
/// deals, not from an independent raw class frequency. This preserves the
/// effect of the opponent range and cross-hand blockers.
pub fn aggregate_strategy_report_from_ranges(
    report: &StrategyReport,
    ranges: [&WeightedRange; 2],
    dead_cards: DeckMask,
) -> Result<RangeStrategyReport, String> {
    let deals = private_deals_from_ranges(ranges, dead_cards)?;
    aggregate_strategy_report_by_deals(report, &deals)
}

pub fn aggregate_strategy_report_by_deals(
    report: &StrategyReport,
    deals: &[PrivateDeal],
) -> Result<RangeStrategyReport, String> {
    if deals.is_empty() {
        return Err("private deal list cannot be empty".to_string());
    }
    let mut total_probability = 0.0;
    let mut marginal = HashMap::<(PlayerId, Combo), f64>::new();
    for deal in deals {
        if deal.probability <= 0.0 || !deal.probability.is_finite() {
            return Err("private deal probability must be finite and positive".to_string());
        }
        HoldemChipEvPayoff::new(deal.hands.to_vec(), [0, 1])?;
        total_probability += deal.probability;
        for (player, &hand) in deal.hands.iter().enumerate() {
            *marginal.entry((player, hand)).or_insert(0.0) += deal.probability;
        }
    }
    if total_probability <= 0.0 || !total_probability.is_finite() {
        return Err("private deal probabilities have invalid total".to_string());
    }
    for weight in marginal.values_mut() {
        *weight /= total_probability;
    }

    let mut accumulators =
        BTreeMap::<(PlayerId, Option<TreeNodeId>, HandClassId), RangeAccumulator>::new();
    let mut seen_infosets = HashSet::new();
    for infoset in &report.infosets {
        let hand = infoset.private_hand.ok_or_else(|| {
            "range aggregation requires a private-card-conditioned strategy report".to_string()
        })?;
        if !seen_infosets.insert((infoset.player, infoset.infoset)) {
            return Err(format!(
                "strategy report contains duplicate information set {}:{}",
                infoset.player, infoset.infoset
            ));
        }
        let weight = marginal
            .get(&(infoset.player, hand))
            .copied()
            .unwrap_or(0.0);
        if weight <= 0.0 {
            return Err(format!(
                "private hand {hand:?} for player {} is absent from supplied deals",
                infoset.player
            ));
        }
        let key = (infoset.player, infoset.public_node, hand.class_id());
        let accumulator = accumulators
            .entry(key)
            .or_insert_with(|| RangeAccumulator::new(infoset));
        if accumulator.actions
            != infoset
                .actions
                .iter()
                .map(|action| action.action.clone())
                .collect::<Vec<_>>()
        {
            return Err(format!(
                "class bucket at player {} node {:?} has inconsistent actions",
                infoset.player, infoset.public_node
            ));
        }
        if !accumulator.combos.insert(hand) {
            return Err(format!(
                "duplicate exact combo in class bucket at player {} node {:?}",
                infoset.player, infoset.public_node
            ));
        }
        accumulator.combo_count += 1;
        accumulator.marginal_weight += weight;
        for (index, action) in infoset.actions.iter().enumerate() {
            if !action.frequency.is_finite() || action.frequency < 0.0 {
                return Err(format!(
                    "invalid action frequency in information set {}:{}",
                    infoset.player, infoset.infoset
                ));
            }
            accumulator.frequency_sums[index] += weight * action.frequency;
            if let Some(value) = action.counterfactual_ev {
                accumulator.ev_sums[index] += weight * value;
                accumulator.ev_weights[index] += weight;
            }
            if let Some(value) = action.ev_loss {
                accumulator.loss_sums[index] += weight * value;
                accumulator.loss_weights[index] += weight;
            }
        }
    }

    let mut classes = Vec::with_capacity(accumulators.len());
    for ((player, public_node, class_id), accumulator) in accumulators {
        if accumulator.marginal_weight <= 0.0 {
            continue;
        }
        let actions = accumulator
            .actions
            .into_iter()
            .enumerate()
            .map(|(index, action)| RangeActionReport {
                action,
                frequency: accumulator.frequency_sums[index] / accumulator.marginal_weight,
                counterfactual_ev: (accumulator.ev_weights[index] > 0.0)
                    .then_some(accumulator.ev_sums[index] / accumulator.ev_weights[index]),
                ev_loss: (accumulator.loss_weights[index] > 0.0)
                    .then_some(accumulator.loss_sums[index] / accumulator.loss_weights[index]),
            })
            .collect();
        classes.push(RangeClassReport {
            player,
            public_node,
            class_id,
            combo_count: accumulator.combo_count,
            marginal_weight: accumulator.marginal_weight,
            actions,
        });
    }

    Ok(RangeStrategyReport {
        algorithm: report.algorithm,
        iterations: report.iterations,
        average_utility: report.average_utility,
        classes,
    })
}

#[derive(Debug, Clone)]
struct RangeAccumulator {
    actions: Vec<Action>,
    combos: HashSet<Combo>,
    combo_count: usize,
    marginal_weight: f64,
    frequency_sums: Vec<f64>,
    ev_sums: Vec<f64>,
    ev_weights: Vec<f64>,
    loss_sums: Vec<f64>,
    loss_weights: Vec<f64>,
}

impl RangeAccumulator {
    fn new(infoset: &InfoSetReport) -> Self {
        let actions: Vec<Action> = infoset
            .actions
            .iter()
            .map(|action| action.action.clone())
            .collect();
        let action_count = actions.len();
        Self {
            actions,
            combos: HashSet::new(),
            combo_count: 0,
            marginal_weight: 0.0,
            frequency_sums: vec![0.0; action_count],
            ev_sums: vec![0.0; action_count],
            ev_weights: vec![0.0; action_count],
            loss_sums: vec![0.0; action_count],
            loss_weights: vec![0.0; action_count],
        }
    }
}

/// Builds a deterministic strategy/action-EV report from a solver checkpoint.
///
/// Frequencies come from the checkpoint's average strategy. Action EVs are
/// counterfactual values under the same average profile, weighted by chance and
/// opponent reach. The report is intentionally exact over the compiled finite
/// game; for very large trees it should be generated as a separate job.
pub fn build_strategy_report(
    compiled: &CompiledHoldemGame,
    checkpoint: &SolverCheckpoint,
) -> Result<StrategyReport, String> {
    checkpoint.validate()?;
    if checkpoint.game_fingerprint != compiled.game().fingerprint() {
        return Err("checkpoint game fingerprint does not match compiled game".to_string());
    }

    let mut metadata = HashMap::<(PlayerId, InfoSetId), InfoSetMetadata>::new();
    for (node_id, node) in compiled.game().nodes().iter().enumerate() {
        let GameNode::Decision {
            player,
            infoset,
            children: _,
        } = node
        else {
            continue;
        };
        let actions = compiled
            .actions_at(node_id)
            .ok_or_else(|| format!("missing action metadata for solver node {node_id}"))?
            .to_vec();
        if actions.is_empty() {
            return Err(format!("information set {player}:{infoset} has no actions"));
        }
        let entry = metadata
            .entry((*player, *infoset))
            .or_insert_with(|| InfoSetMetadata {
                player: *player,
                infoset: *infoset,
                public_node: compiled.tree_node_id(node_id),
                private_hand: compiled
                    .private_hands_at(node_id)
                    .map(|hands| hands[*player]),
                actions: actions.clone(),
                occurrences: 0,
            });
        if entry.actions != actions {
            return Err(format!(
                "information set {player}:{infoset} has inconsistent action metadata"
            ));
        }
        if entry.public_node != compiled.tree_node_id(node_id)
            || entry.private_hand
                != compiled
                    .private_hands_at(node_id)
                    .map(|hands| hands[*player])
        {
            return Err(format!(
                "information set {player}:{infoset} has inconsistent private/public metadata"
            ));
        }
        entry.occurrences += 1;
    }

    let mut checkpoint_infosets = HashMap::new();
    for entry in &checkpoint.infosets {
        let key = (entry.player, entry.infoset);
        let metadata_entry = metadata.get(&key).ok_or_else(|| {
            format!(
                "checkpoint references information set {}:{} absent from compiled game",
                entry.player, entry.infoset
            )
        })?;
        if entry.regret_sum.len() != metadata_entry.actions.len()
            || entry.strategy_sum.len() != metadata_entry.actions.len()
        {
            return Err(format!(
                "checkpoint action count mismatch for information set {}:{}",
                entry.player, entry.infoset
            ));
        }
        checkpoint_infosets.insert(key, entry);
    }

    let mut strategies = HashMap::new();
    for (&key, entry) in &metadata {
        let checkpoint_entry = checkpoint_infosets.get(&key).copied();
        strategies.insert(
            key,
            average_strategy_from_checkpoint(checkpoint_entry, entry.actions.len()),
        );
    }
    let average_utility =
        evaluate_average_policy(compiled.game(), &strategies, compiled.game().root())?;

    let mut reports = Vec::with_capacity(metadata.len());
    let mut metadata_values: Vec<_> = metadata.into_values().collect();
    metadata_values.sort_by_key(|entry| (entry.player, entry.infoset));
    for entry in metadata_values {
        let key = (entry.player, entry.infoset);
        let frequencies = strategies
            .get(&key)
            .ok_or_else(|| "strategy metadata disappeared".to_string())?;
        let action_values = counterfactual_action_values(compiled.game(), &strategies, key)?;
        let best_ev = action_values
            .iter()
            .flatten()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let visits = checkpoint_infosets
            .get(&key)
            .map(|entry| entry.visits)
            .unwrap_or(0);
        let actions = entry
            .actions
            .into_iter()
            .enumerate()
            .map(|(index, action)| {
                let counterfactual_ev = action_values[index];
                ActionReport {
                    action,
                    frequency: frequencies[index],
                    counterfactual_ev,
                    ev_loss: counterfactual_ev.map(|value| best_ev - value),
                }
            })
            .collect();
        reports.push(InfoSetReport {
            player: entry.player,
            infoset: entry.infoset,
            public_node: entry.public_node,
            private_hand: entry.private_hand,
            occurrences: entry.occurrences,
            visits,
            actions,
        });
    }

    Ok(StrategyReport {
        algorithm: checkpoint.algorithm,
        iterations: checkpoint.iterations,
        average_utility,
        infosets: reports,
    })
}

#[derive(Debug, Clone)]
struct InfoSetMetadata {
    player: PlayerId,
    infoset: InfoSetId,
    public_node: Option<TreeNodeId>,
    private_hand: Option<Combo>,
    actions: Vec<Action>,
    occurrences: usize,
}

fn average_strategy_from_checkpoint(
    entry: Option<&InfoSetCheckpoint>,
    action_count: usize,
) -> Vec<f64> {
    let Some(entry) = entry else {
        return vec![1.0 / action_count as f64; action_count];
    };
    let total: f64 = entry.strategy_sum.iter().sum();
    if total <= 0.0 {
        vec![1.0 / action_count as f64; action_count]
    } else {
        entry
            .strategy_sum
            .iter()
            .map(|value| value / total)
            .collect()
    }
}

fn evaluate_average_policy(
    game: &StaticGame,
    strategies: &HashMap<(PlayerId, InfoSetId), Vec<f64>>,
    node_id: NodeId,
) -> Result<[f64; 2], String> {
    let node = game
        .node(node_id)
        .ok_or_else(|| format!("unknown game node {node_id}"))?;
    match node {
        GameNode::Terminal { utility } => Ok(*utility),
        GameNode::Chance { outcomes } => {
            let mut utility = [0.0, 0.0];
            for &(probability, child) in outcomes {
                let child_utility = evaluate_average_policy(game, strategies, child)?;
                utility[0] += probability * child_utility[0];
                utility[1] += probability * child_utility[1];
            }
            Ok(utility)
        }
        GameNode::Decision {
            player,
            infoset,
            children,
        } => {
            let strategy = strategies.get(&(*player, *infoset)).ok_or_else(|| {
                format!("missing strategy for information set {player}:{infoset}")
            })?;
            if strategy.len() != children.len() {
                return Err(format!(
                    "strategy/action count mismatch for information set {player}:{infoset}"
                ));
            }
            let mut utility = [0.0, 0.0];
            for (&probability, &child) in strategy.iter().zip(children) {
                let child_utility = evaluate_average_policy(game, strategies, child)?;
                utility[0] += probability * child_utility[0];
                utility[1] += probability * child_utility[1];
            }
            Ok(utility)
        }
    }
}

fn counterfactual_action_values(
    game: &StaticGame,
    strategies: &HashMap<(PlayerId, InfoSetId), Vec<f64>>,
    target: (PlayerId, InfoSetId),
) -> Result<Vec<Option<f64>>, String> {
    let target_player = target.0;
    let action_count = game
        .nodes()
        .iter()
        .find_map(|node| match node {
            GameNode::Decision {
                player,
                infoset,
                children,
            } if (*player, *infoset) == target => Some(children.len()),
            _ => None,
        })
        .ok_or_else(|| format!("unknown target information set {}:{}", target.0, target.1))?;
    let mut weighted_values = vec![0.0; action_count];
    let mut counterfactual_reach = 0.0;
    collect_counterfactual_values(
        game,
        strategies,
        target,
        target_player,
        game.root(),
        [1.0, 1.0],
        1.0,
        &mut weighted_values,
        &mut counterfactual_reach,
    )?;
    if counterfactual_reach <= 0.0 {
        return Ok(vec![None; action_count]);
    }
    Ok(weighted_values
        .into_iter()
        .map(|value| Some(value / counterfactual_reach))
        .collect())
}

fn collect_counterfactual_values(
    game: &StaticGame,
    strategies: &HashMap<(PlayerId, InfoSetId), Vec<f64>>,
    target: (PlayerId, InfoSetId),
    target_player: PlayerId,
    node_id: NodeId,
    reach: [f64; 2],
    chance_reach: f64,
    weighted_values: &mut [f64],
    counterfactual_reach: &mut f64,
) -> Result<(), String> {
    let node = game
        .node(node_id)
        .ok_or_else(|| format!("unknown game node {node_id}"))?;
    match node {
        GameNode::Terminal { .. } => Ok(()),
        GameNode::Chance { outcomes } => {
            for &(probability, child) in outcomes {
                collect_counterfactual_values(
                    game,
                    strategies,
                    target,
                    target_player,
                    child,
                    reach,
                    chance_reach * probability,
                    weighted_values,
                    counterfactual_reach,
                )?;
            }
            Ok(())
        }
        GameNode::Decision {
            player,
            infoset,
            children,
        } => {
            let strategy = strategies.get(&(*player, *infoset)).ok_or_else(|| {
                format!("missing strategy for information set {player}:{infoset}")
            })?;
            if strategy.len() != children.len() {
                return Err(format!(
                    "strategy/action count mismatch for information set {player}:{infoset}"
                ));
            }
            if (*player, *infoset) == target {
                let weight = reach[1 - target_player] * chance_reach;
                *counterfactual_reach += weight;
                for (action, &child) in children.iter().enumerate() {
                    let utility = evaluate_average_policy(game, strategies, child)?;
                    weighted_values[action] += weight * utility[target_player];
                }
                return Ok(());
            }
            for (&probability, &child) in strategy.iter().zip(children) {
                let mut next_reach = reach;
                if *player != target_player {
                    next_reach[*player] *= probability;
                }
                collect_counterfactual_values(
                    game,
                    strategies,
                    target,
                    target_player,
                    child,
                    next_reach,
                    chance_reach,
                    weighted_values,
                    counterfactual_reach,
                )?;
            }
            Ok(())
        }
    }
}

/// Runs CFR+ or external-sampling MCCFR on an already compiled finite-deal
/// Hold'em game. `config_fingerprint` should be derived from the tree/action
/// abstraction configuration, for example `FullTreeBuildConfig::fingerprint()`.
pub fn run_compiled_holdem_game(
    compiled: &CompiledHoldemGame,
    algorithm: SolverAlgorithm,
    iterations: u64,
    seed: u64,
    config_fingerprint: u64,
) -> Result<HoldemSolveResult, String> {
    let (checkpoint, average_utility) = match algorithm {
        SolverAlgorithm::CfrPlus => {
            let mut solver = CfrPlusSolver::new_with_config_fingerprint(
                compiled.game().clone(),
                config_fingerprint,
            )?;
            solver.run(iterations)?;
            let utility = solver.evaluate_average_strategy()?;
            (solver.checkpoint(), utility)
        }
        SolverAlgorithm::ExternalSamplingMccfr => {
            let mut solver = MccfrSolver::new_with_config_fingerprint(
                compiled.game().clone(),
                seed,
                config_fingerprint,
            )?;
            solver.run(iterations)?;
            let utility = solver.evaluate_average_strategy()?;
            (solver.checkpoint(), utility)
        }
    };
    Ok(HoldemSolveResult {
        checkpoint,
        average_utility,
        game_nodes: compiled.node_count(),
        private_deals: compiled.private_deal_count(),
    })
}

/// Resumes a compiled finite-deal Hold'em game from a compatible checkpoint.
pub fn resume_compiled_holdem_game(
    compiled: &CompiledHoldemGame,
    checkpoint: &SolverCheckpoint,
    additional_iterations: u64,
) -> Result<HoldemSolveResult, String> {
    resume_compiled_holdem_game_with_config_fingerprint(
        compiled,
        checkpoint,
        additional_iterations,
        checkpoint.config_fingerprint,
    )
}

pub fn resume_compiled_holdem_game_with_config_fingerprint(
    compiled: &CompiledHoldemGame,
    checkpoint: &SolverCheckpoint,
    additional_iterations: u64,
    config_fingerprint: u64,
) -> Result<HoldemSolveResult, String> {
    let (checkpoint, average_utility) = match checkpoint.algorithm {
        SolverAlgorithm::CfrPlus => {
            let mut solver = CfrPlusSolver::from_checkpoint_with_config_fingerprint(
                compiled.game().clone(),
                checkpoint,
                config_fingerprint,
            )?;
            solver.run(additional_iterations)?;
            let utility = solver.evaluate_average_strategy()?;
            (solver.checkpoint(), utility)
        }
        SolverAlgorithm::ExternalSamplingMccfr => {
            let mut solver = MccfrSolver::from_checkpoint_with_config_fingerprint(
                compiled.game().clone(),
                checkpoint,
                config_fingerprint,
            )?;
            solver.run(additional_iterations)?;
            let utility = solver.evaluate_average_strategy()?;
            (solver.checkpoint(), utility)
        }
    };
    Ok(HoldemSolveResult {
        checkpoint,
        average_utility,
        game_nodes: compiled.node_count(),
        private_deals: compiled.private_deal_count(),
    })
}

/// Expands ranges, compiles blocker-conditioned private deals, and runs one
/// finite-deal Hold'em batch in a single call.
pub fn solve_private_holdem_from_ranges(
    tree: &GameTree,
    ranges: [&WeightedRange; 2],
    dead_cards: DeckMask,
    algorithm: SolverAlgorithm,
    iterations: u64,
    seed: u64,
    config_fingerprint: u64,
) -> Result<HoldemSolveResult, String> {
    let deals = private_deals_from_ranges(ranges, dead_cards)?;
    let compiled = compile_private_holdem_tree(tree, &deals)?;
    run_compiled_holdem_game(&compiled, algorithm, iterations, seed, config_fingerprint)
}

/// Rebuilds the deterministic range-conditioned game and resumes a previous
/// batch. Any changed ranges, board chance model, or tree is rejected by the
/// game/config fingerprints during restore.
pub fn resume_private_holdem_from_ranges(
    tree: &GameTree,
    ranges: [&WeightedRange; 2],
    dead_cards: DeckMask,
    checkpoint: &SolverCheckpoint,
    additional_iterations: u64,
) -> Result<HoldemSolveResult, String> {
    resume_private_holdem_from_ranges_with_config_fingerprint(
        tree,
        ranges,
        dead_cards,
        checkpoint,
        additional_iterations,
        checkpoint.config_fingerprint,
    )
}

pub fn resume_private_holdem_from_ranges_with_config_fingerprint(
    tree: &GameTree,
    ranges: [&WeightedRange; 2],
    dead_cards: DeckMask,
    checkpoint: &SolverCheckpoint,
    additional_iterations: u64,
    config_fingerprint: u64,
) -> Result<HoldemSolveResult, String> {
    let deals = private_deals_from_ranges(ranges, dead_cards)?;
    let compiled = compile_private_holdem_tree(tree, &deals)?;
    resume_compiled_holdem_game_with_config_fingerprint(
        &compiled,
        checkpoint,
        additional_iterations,
        config_fingerprint,
    )
}

/// Compiles a validated public-information `GameTree` into the generic solver
/// graph. Decision nodes use a unique information set, which is correct for a
/// perfect-information toy or a fixed-board debugging tree, but not yet for
/// a real private-card Hold'em abstraction.
pub fn compile_holdem_tree<P: TerminalPayoff>(
    tree: &GameTree,
    payoff: &P,
) -> Result<CompiledHoldemGame, String> {
    tree.validate()?;
    validate_reachable_acyclic_tree(tree)?;

    let mut nodes = Vec::with_capacity(tree.nodes.len());
    let mut actions = vec![Vec::new(); tree.nodes.len()];

    for tree_node in &tree.nodes {
        let game_node = if let Some(chance) = &tree_node.chance {
            if tree_node.leaf.is_some() {
                return Err(format!(
                    "tree node {} is both chance and leaf",
                    tree_node.id
                ));
            }
            if chance.outcomes.len() != tree_node.children.len() {
                return Err(format!(
                    "chance node {} has mismatched outcomes and children",
                    tree_node.id
                ));
            }
            GameNode::Chance {
                outcomes: chance
                    .outcomes
                    .iter()
                    .zip(tree_node.children.iter().copied())
                    .map(|(outcome, child)| (outcome.probability, child))
                    .collect(),
            }
        } else if let Some(leaf) = &tree_node.leaf {
            match leaf {
                LeafKind::Terminal(_) => GameNode::Terminal {
                    utility: payoff.utility(tree_node)?,
                },
                LeafKind::RoundComplete => {
                    return Err(format!(
                        "round-complete node {} has no terminal payoff",
                        tree_node.id
                    ));
                }
            }
        } else {
            let player = tree_node
                .state
                .actor
                .ok_or_else(|| format!("decision node {} has no actor", tree_node.id))?;
            if player > 1 {
                return Err(format!(
                    "generic solver supports players 0 and 1, got player {player}"
                ));
            }

            let mut node_actions = Vec::with_capacity(tree_node.children.len());
            for &child_id in &tree_node.children {
                let child = tree
                    .node(child_id)
                    .ok_or_else(|| format!("unknown child node {child_id}"))?;
                let action = child.action_from_parent.clone().ok_or_else(|| {
                    format!(
                        "decision edge {} -> {} has no action",
                        tree_node.id, child_id
                    )
                })?;
                if node_actions.contains(&action) {
                    return Err(format!(
                        "decision node {} contains duplicate action {action:?}",
                        tree_node.id
                    ));
                }
                node_actions.push(action);
            }
            if node_actions.is_empty() {
                return Err(format!("decision node {} has no actions", tree_node.id));
            }
            actions[tree_node.id] = node_actions;
            GameNode::Decision {
                player,
                infoset: perfect_information_infoset(tree_node.id),
                children: tree_node.children.clone(),
            }
        };
        nodes.push(game_node);
    }

    let game = StaticGame::new(tree.root, nodes)?;
    Ok(CompiledHoldemGame {
        game,
        actions,
        tree_node_ids: (0..tree.nodes.len()).map(Some).collect(),
        private_hands: vec![None; tree.nodes.len()],
    })
}

/// Compiles the same public tree for a finite private-card deal list.
///
/// The root becomes a private-card chance node. Each copied decision node is
/// keyed by `(public tree node, acting player, acting player's exact Combo)`;
/// therefore branches with different opponent hands share strategy while
/// branches with different private cards do not. Public chance outcomes that
/// conflict with a deal are removed and renormalized. This is the first
/// private-card conditioning layer and is intentionally finite-deal based.
pub fn compile_private_holdem_tree(
    tree: &GameTree,
    deals: &[PrivateDeal],
) -> Result<CompiledHoldemGame, String> {
    tree.validate()?;
    validate_reachable_acyclic_tree(tree)?;
    if deals.is_empty() {
        return Err("private deal list cannot be empty".to_string());
    }

    let mut total_probability = 0.0;
    for deal in deals {
        if deal.probability <= 0.0 || !deal.probability.is_finite() {
            return Err("private deal probability must be finite and positive".to_string());
        }
        HoldemChipEvPayoff::new(deal.hands.to_vec(), [0, 1])?;
        total_probability += deal.probability;
    }
    if total_probability <= 0.0 || !total_probability.is_finite() {
        return Err("private deal probabilities have invalid total".to_string());
    }

    let mut nodes = vec![GameNode::Chance {
        outcomes: Vec::new(),
    }];
    let mut actions = vec![Vec::new()];
    let mut tree_node_ids = vec![None];
    let mut private_hands = vec![None];
    let mut infoset_ids = HashMap::new();
    let mut outcomes = Vec::with_capacity(deals.len());

    for deal in deals {
        let payoff = HoldemChipEvPayoff::new(deal.hands.to_vec(), [0, 1])?;
        let branch = append_private_node(
            tree,
            tree.root,
            deal.hands,
            &payoff,
            &mut nodes,
            &mut actions,
            &mut tree_node_ids,
            &mut private_hands,
            &mut infoset_ids,
        )?;
        outcomes.push((deal.probability / total_probability, branch));
    }
    nodes[0] = GameNode::Chance { outcomes };

    let game = StaticGame::new(0, nodes)?;
    Ok(CompiledHoldemGame {
        game,
        actions,
        tree_node_ids,
        private_hands,
    })
}

fn append_private_node(
    tree: &GameTree,
    tree_node_id: TreeNodeId,
    hands: [Combo; 2],
    payoff: &HoldemChipEvPayoff,
    nodes: &mut Vec<GameNode>,
    actions: &mut Vec<Vec<Action>>,
    tree_node_ids: &mut Vec<Option<TreeNodeId>>,
    private_hands: &mut Vec<Option<[Combo; 2]>>,
    infoset_ids: &mut HashMap<(TreeNodeId, PlayerId, Combo), InfoSetId>,
) -> Result<NodeId, String> {
    let tree_node = tree
        .node(tree_node_id)
        .ok_or_else(|| format!("unknown public tree node {tree_node_id}"))?;
    let hands_mask = hands[0].mask() | hands[1].mask();
    let board_mask = mask_from_cards(&tree_node.state.board).map_err(|error| error.to_string())?;
    if hands_mask & board_mask != 0 {
        return Err(format!(
            "private deal conflicts with public board at tree node {}",
            tree_node.id
        ));
    }
    let solver_node_id = nodes.len();
    nodes.push(GameNode::Terminal {
        utility: [0.0, 0.0],
    });
    actions.push(Vec::new());
    tree_node_ids.push(Some(tree_node_id));
    private_hands.push(Some(hands));

    let compiled = if let Some(chance) = &tree_node.chance {
        let mut allowed = Vec::with_capacity(chance.outcomes.len());
        let mut probability_total = 0.0;
        for (outcome, &child) in chance.outcomes.iter().zip(&tree_node.children) {
            let outcome_mask =
                mask_from_cards(&outcome.cards).map_err(|error| error.to_string())?;
            if outcome_mask & hands_mask != 0 {
                continue;
            }
            probability_total += outcome.probability;
            allowed.push((outcome.probability, child));
        }
        if allowed.is_empty() || probability_total <= 0.0 || !probability_total.is_finite() {
            return Err(format!(
                "private deal has no legal public-card outcomes at tree node {}",
                tree_node.id
            ));
        }
        let outcomes = allowed
            .into_iter()
            .map(|(probability, child)| {
                let child_id = append_private_node(
                    tree,
                    child,
                    hands,
                    payoff,
                    nodes,
                    actions,
                    tree_node_ids,
                    private_hands,
                    infoset_ids,
                )?;
                Ok((probability / probability_total, child_id))
            })
            .collect::<Result<Vec<_>, String>>()?;
        GameNode::Chance { outcomes }
    } else if let Some(leaf) = &tree_node.leaf {
        match leaf {
            LeafKind::Terminal(_) => GameNode::Terminal {
                utility: payoff.utility(tree_node)?,
            },
            LeafKind::RoundComplete => {
                return Err(format!(
                    "round-complete node {} has no terminal payoff",
                    tree_node.id
                ));
            }
        }
    } else {
        let player = tree_node
            .state
            .actor
            .ok_or_else(|| format!("decision node {} has no actor", tree_node.id))?;
        if player > 1 {
            return Err(format!(
                "generic solver supports players 0 and 1, got player {player}"
            ));
        }

        let mut node_actions = Vec::with_capacity(tree_node.children.len());
        let mut child_ids = Vec::with_capacity(tree_node.children.len());
        for &child in &tree_node.children {
            let child_node = tree
                .node(child)
                .ok_or_else(|| format!("unknown child node {child}"))?;
            let action = child_node.action_from_parent.clone().ok_or_else(|| {
                format!("decision edge {} -> {} has no action", tree_node.id, child)
            })?;
            if node_actions.contains(&action) {
                return Err(format!(
                    "decision node {} contains duplicate action {action:?}",
                    tree_node.id
                ));
            }
            node_actions.push(action);
            child_ids.push(append_private_node(
                tree,
                child,
                hands,
                payoff,
                nodes,
                actions,
                tree_node_ids,
                private_hands,
                infoset_ids,
            )?);
        }
        if node_actions.is_empty() {
            return Err(format!("decision node {} has no actions", tree_node.id));
        }
        actions[solver_node_id] = node_actions;
        let key = (tree_node.id, player, hands[player]);
        let infoset = if let Some(&existing) = infoset_ids.get(&key) {
            existing
        } else {
            let next = infoset_ids.len() as InfoSetId;
            infoset_ids.insert(key, next);
            next
        };
        GameNode::Decision {
            player,
            infoset,
            children: child_ids,
        }
    };

    nodes[solver_node_id] = compiled;
    Ok(solver_node_id)
}

fn perfect_information_infoset(tree_node_id: TreeNodeId) -> InfoSetId {
    tree_node_id as InfoSetId
}

fn validate_reachable_acyclic_tree(tree: &GameTree) -> Result<(), String> {
    let mut marks = vec![0u8; tree.nodes.len()];
    visit_tree(tree, tree.root, &mut marks)?;
    if marks.iter().any(|mark| *mark != 2) {
        return Err("tree contains a node unreachable from root".to_string());
    }
    Ok(())
}

fn visit_tree(tree: &GameTree, node_id: TreeNodeId, marks: &mut [u8]) -> Result<(), String> {
    match marks.get(node_id).copied() {
        Some(1) => return Err(format!("tree contains a cycle at node {node_id}")),
        Some(2) => return Ok(()),
        Some(0) => {}
        None => return Err(format!("tree references invalid node {node_id}")),
        Some(_) => return Err(format!("tree has invalid traversal mark at node {node_id}")),
    }
    marks[node_id] = 1;
    for &child in &tree.nodes[node_id].children {
        visit_tree(tree, child, marks)?;
    }
    marks[node_id] = 2;
    Ok(())
}

/// Fixed private hands plus exact ChipEV terminal settlement for a heads-up
/// Hold'em tree. Fold terminals are deterministic; showdown terminals use the
/// existing side-pot/equity settlement API and therefore also handle a
/// showdown that ends before the river because of an all-in.
#[derive(Debug, Clone)]
pub struct HoldemChipEvPayoff {
    hands: Vec<Combo>,
    solver_seats: [PlayerId; 2],
}

impl HoldemChipEvPayoff {
    pub fn new(hands: Vec<Combo>, solver_seats: [PlayerId; 2]) -> Result<Self, String> {
        if hands.len() != 2 {
            return Err("the first Hold'em payoff adapter supports exactly two hands".to_string());
        }
        if solver_seats[0] == solver_seats[1] {
            return Err("solver seats must be distinct".to_string());
        }
        let mut used_cards = 0u64;
        for hand in &hands {
            if hand.mask() & used_cards != 0 {
                return Err("private hands contain duplicate cards".to_string());
            }
            used_cards |= hand.mask();
        }
        Ok(Self {
            hands,
            solver_seats,
        })
    }

    pub fn hands(&self) -> &[Combo] {
        &self.hands
    }

    pub fn solver_seats(&self) -> [PlayerId; 2] {
        self.solver_seats
    }

    fn validate_state(&self, node: &TreeNode) -> Result<(), String> {
        if node.state.table_size != 2 || node.state.players.len() != 2 {
            return Err(
                "the first Hold'em payoff adapter supports heads-up states only".to_string(),
            );
        }
        if self
            .solver_seats
            .iter()
            .any(|&seat| seat >= node.state.table_size)
        {
            return Err("solver seat is not present in the state".to_string());
        }
        Ok(())
    }
}

impl TerminalPayoff for HoldemChipEvPayoff {
    fn utility(&self, node: &TreeNode) -> Result<[f64; 2], String> {
        self.validate_state(node)?;
        let terminal = match node.leaf.as_ref() {
            Some(LeafKind::Terminal(terminal)) => terminal,
            Some(LeafKind::RoundComplete) => {
                return Err("round-complete node cannot receive terminal utility".to_string())
            }
            None => return Err("utility requested for a non-terminal node".to_string()),
        };

        match terminal {
            TerminalState::Fold { winner } => {
                if *winner >= node.state.table_size {
                    return Err(format!("fold winner is out of range: {winner}"));
                }
                let mut utility = [0.0; 2];
                for (index, &seat) in self.solver_seats.iter().enumerate() {
                    let payout = if seat == *winner {
                        node.state.pot as f64
                    } else {
                        0.0
                    };
                    utility[index] = payout - node.state.players[seat].committed_total as f64;
                }
                Ok(utility)
            }
            TerminalState::Showdown => {
                let settlement = exact_chip_ev_showdown(&node.state, &self.hands)?;
                Ok([
                    settlement.net_ev[self.solver_seats[0]],
                    settlement.net_ev[self.solver_seats[1]],
                ])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CfrPlusSolver;
    use holdem_cards::{cards_from_str, mask_from_cards};
    use holdem_domain::setup::build_preflop_state;
    use holdem_domain::table::{AnteMode, TableConfig};
    use holdem_domain::{Action, ActionSizes, GameState, PlayerState, Street};
    use holdem_ranges::{WeightedCombo, WeightedRange};
    use holdem_tree::{
        ChanceConfig, FullTreeBuildConfig, GameTree, TreeBuildConfig, TreeBuilder, TreeNode,
    };

    fn combo(text: &str) -> Combo {
        let cards = cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    fn terminal_tree(terminal: TerminalState) -> GameTree {
        let mut state = GameState::new(
            2,
            Street::River,
            cards_from_str("2s 7d 9c Kh Qh").unwrap(),
            vec![
                PlayerState::new(0, 900).unwrap(),
                PlayerState::new(1, 900).unwrap(),
            ],
            0,
        )
        .unwrap();
        state.players[0].committed_total = 100;
        state.players[0].committed_street = 100;
        state.players[1].committed_total = 100;
        state.players[1].committed_street = 100;
        state.recompute_pot().unwrap();
        state.terminal = Some(terminal.clone());
        GameTree {
            root: 0,
            nodes: vec![TreeNode {
                id: 0,
                parent: None,
                action_from_parent: None,
                state,
                children: Vec::new(),
                leaf: Some(LeafKind::Terminal(terminal)),
                chance: None,
            }],
        }
    }

    #[test]
    fn fold_payoff_is_pot_payout_less_contribution() {
        let tree = terminal_tree(TerminalState::Fold { winner: 0 });
        let payoff = HoldemChipEvPayoff::new(vec![combo("As Ah"), combo("Jc Js")], [0, 1]).unwrap();
        let compiled = compile_holdem_tree(&tree, &payoff).unwrap();
        assert_eq!(compiled.game().nodes().len(), 1);
        match compiled.game().node(0).unwrap() {
            GameNode::Terminal { utility } => assert_eq!(*utility, [100.0, -100.0]),
            other => panic!("expected terminal node, got {other:?}"),
        }
    }

    #[test]
    fn showdown_payoff_uses_exact_chip_ev_settlement() {
        let tree = terminal_tree(TerminalState::Showdown);
        let payoff = HoldemChipEvPayoff::new(vec![combo("As Ah"), combo("Jc Js")], [0, 1]).unwrap();
        let compiled = compile_holdem_tree(&tree, &payoff).unwrap();
        let GameNode::Terminal { utility } = compiled.game().node(0).unwrap() else {
            panic!("expected terminal node")
        };
        assert!(utility[0] > utility[1]);
        assert!((utility[0] + utility[1]).abs() < 1e-9);
    }

    #[test]
    fn public_holdem_tree_compiles_and_preserves_actions() {
        let table = TableConfig {
            table_size: 2,
            button: 0,
            small_blind: 500,
            big_blind: 1_000,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![10_000, 10_000],
            dead_money: 0,
        };
        let tree_config = FullTreeBuildConfig {
            round: TreeBuildConfig {
                action_sizes: ActionSizes::default(),
                abstraction: None,
                max_nodes: 10_000,
                max_depth: 64,
            },
            chance: ChanceConfig {
                flop: Some(vec![holdem_tree::ChanceOutcome::new(
                    cards_from_str("2s 7d 9c").unwrap(),
                    1.0,
                )]),
                turn: Some(vec![holdem_tree::ChanceOutcome::new(
                    cards_from_str("Kh").unwrap(),
                    1.0,
                )]),
                river: Some(vec![holdem_tree::ChanceOutcome::new(
                    cards_from_str("Qh").unwrap(),
                    1.0,
                )]),
                enumerate_exact: false,
                max_outcomes_per_node: 10,
            },
            postflop_order: vec![1, 0],
        };
        let tree =
            TreeBuilder::build_full(build_preflop_state(&table).unwrap(), &tree_config).unwrap();
        let payoff = HoldemChipEvPayoff::new(vec![combo("As Ah"), combo("Jc Js")], [0, 1]).unwrap();
        let compiled = compile_holdem_tree(&tree, &payoff).unwrap();

        assert_eq!(compiled.game().nodes().len(), tree.nodes.len());
        let root_actions = compiled.actions_at(tree.root).unwrap();
        assert!(root_actions.contains(&Action::Fold));
        assert!(root_actions.contains(&Action::Call));
        assert!(compiled
            .game()
            .nodes()
            .iter()
            .any(|node| matches!(node, GameNode::Chance { .. })));
    }

    #[test]
    fn private_deal_compilation_shares_only_matching_private_information() {
        let table = TableConfig {
            table_size: 2,
            button: 0,
            small_blind: 500,
            big_blind: 1_000,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![10_000, 10_000],
            dead_money: 0,
        };
        let tree_config = FullTreeBuildConfig {
            round: TreeBuildConfig {
                action_sizes: ActionSizes::default(),
                abstraction: None,
                max_nodes: 10_000,
                max_depth: 64,
            },
            chance: ChanceConfig {
                flop: Some(vec![
                    holdem_tree::ChanceOutcome::new(cards_from_str("2s 7d 9c").unwrap(), 1.0),
                    holdem_tree::ChanceOutcome::new(cards_from_str("3s 7h 9d").unwrap(), 1.0),
                ]),
                turn: Some(vec![holdem_tree::ChanceOutcome::new(
                    cards_from_str("Kh").unwrap(),
                    1.0,
                )]),
                river: Some(vec![holdem_tree::ChanceOutcome::new(
                    cards_from_str("Qh").unwrap(),
                    1.0,
                )]),
                enumerate_exact: false,
                max_outcomes_per_node: 10,
            },
            postflop_order: vec![1, 0],
        };
        let tree =
            TreeBuilder::build_full(build_preflop_state(&table).unwrap(), &tree_config).unwrap();
        let deals = [
            PrivateDeal {
                probability: 0.5,
                hands: [combo("As Ah"), combo("Jc Js")],
            },
            PrivateDeal {
                probability: 0.5,
                hands: [combo("As Ah"), combo("2s 3h")],
            },
        ];
        let compiled = compile_private_holdem_tree(&tree, &deals).unwrap();
        let GameNode::Chance { outcomes } = compiled.game().node(0).unwrap() else {
            panic!("expected private-card chance root")
        };
        assert_eq!(outcomes.len(), 2);
        let first_root = outcomes[0].1;
        let second_root = outcomes[1].1;
        let (first_infoset, first_children) = match compiled.game().node(first_root).unwrap() {
            GameNode::Decision {
                player,
                infoset,
                children,
            } => {
                assert_eq!(*player, 0);
                (*infoset, children.clone())
            }
            other => panic!("expected first private decision, got {other:?}"),
        };
        let second_infoset = match compiled.game().node(second_root).unwrap() {
            GameNode::Decision { infoset, .. } => *infoset,
            other => panic!("expected second private decision, got {other:?}"),
        };
        assert_eq!(first_infoset, second_infoset);
        assert_eq!(
            compiled.private_hands_at(first_root).unwrap()[0],
            combo("As Ah")
        );

        let first_after_call = first_children[1];
        let second_after_call = match compiled.game().node(second_root).unwrap() {
            GameNode::Decision { children, .. } => children[1],
            _ => unreachable!(),
        };
        let first_p1_infoset = match compiled.game().node(first_after_call).unwrap() {
            GameNode::Decision { infoset, .. } => *infoset,
            other => panic!("expected player-one decision, got {other:?}"),
        };
        let second_p1_infoset = match compiled.game().node(second_after_call).unwrap() {
            GameNode::Decision { infoset, .. } => *infoset,
            other => panic!("expected player-one decision, got {other:?}"),
        };
        assert_ne!(first_p1_infoset, second_p1_infoset);

        let mut solver = CfrPlusSolver::new(compiled.game().clone()).unwrap();
        solver.run(20).unwrap();
        let value = solver.evaluate_average_strategy().unwrap();
        assert_eq!(solver.iterations(), 20);
        assert!(value[0].is_finite() && value[1].is_finite());
        assert!((value[0] + value[1]).abs() < 1e-9);
        let root_strategy = solver.average_strategy(0, first_infoset).unwrap();
        assert_eq!(root_strategy.len(), 2);
        assert!((root_strategy.iter().sum::<f64>() - 1.0).abs() < 1e-9);

        let report = build_strategy_report(&compiled, &solver.checkpoint()).unwrap();
        assert!(!report.infosets.is_empty());
        assert_eq!(
            report.average_utility,
            solver.evaluate_average_strategy().unwrap()
        );
        let root_report = report
            .infosets
            .iter()
            .find(|entry| entry.player == 0 && entry.infoset == first_infoset)
            .unwrap();
        assert_eq!(root_report.private_hand, Some(combo("As Ah")));
        assert_eq!(root_report.actions.len(), 2);
        assert!(
            (root_report
                .actions
                .iter()
                .map(|action| action.frequency)
                .sum::<f64>()
                - 1.0)
                .abs()
                < 1e-9
        );
        assert!(root_report
            .actions
            .iter()
            .all(|action| action.counterfactual_ev.is_some()));

        let range_report = aggregate_strategy_report_by_deals(&report, &deals).unwrap();
        let root_class = range_report
            .classes
            .iter()
            .find(|class| {
                class.player == 0
                    && class.public_node == Some(tree.root)
                    && class.class_id == combo("As Ah").class_id()
            })
            .unwrap();
        assert_eq!(root_class.combo_count, 1);
        assert!((root_class.marginal_weight - 1.0).abs() < 1e-12);
        assert!(
            (root_class
                .actions
                .iter()
                .map(|action| action.frequency)
                .sum::<f64>()
                - 1.0)
                .abs()
                < 1e-9
        );
        let matrix = range_report.to_matrix().unwrap();
        assert_eq!(matrix.schema_version, RANGE_RESULT_SCHEMA_VERSION);
        assert_eq!(
            matrix.rank_order,
            vec!['A', 'K', 'Q', 'J', 'T', '9', '8', '7', '6', '5', '4', '3', '2']
        );
        let root_matrix = matrix
            .matrices
            .iter()
            .find(|matrix| matrix.player == 0 && matrix.public_node == Some(tree.root))
            .unwrap();
        assert_eq!(
            root_matrix.cells.len(),
            RANGE_MATRIX_SIZE * RANGE_MATRIX_SIZE
        );
        let aa_cell = root_matrix
            .cells
            .iter()
            .find(|cell| cell.class_name == "AA")
            .unwrap();
        assert_eq!(aa_cell.combo_count, Some(1));
        assert!((aa_cell.marginal_weight.unwrap() - 1.0).abs() < 1e-12);
        let aks_cell = root_matrix
            .cells
            .iter()
            .find(|cell| cell.class_name == "AKs")
            .unwrap();
        assert_eq!((aks_cell.row, aks_cell.column), (0, 1));
        assert_eq!(aks_cell.combo_count, None);
        let ako_cell = root_matrix
            .cells
            .iter()
            .find(|cell| cell.class_name == "AKo")
            .unwrap();
        assert_eq!((ako_cell.row, ako_cell.column), (1, 0));

        let json = range_report.to_json().unwrap();
        assert!(json.contains("\"schema_version\""));
        assert!(json.contains("\"class_id\""));
        assert!(json.contains("\"matrices\""));
        assert!(json.contains("\"kind\""));
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
        let csv = range_report.to_csv().unwrap();
        assert!(csv.lines().next().unwrap().contains("class_name"));
        assert!(csv.lines().any(|line| line.contains(",AA,")));
        assert!(csv.lines().count() > RANGE_MATRIX_SIZE * RANGE_MATRIX_SIZE);
    }

    #[test]
    fn range_conditioned_batch_runs_and_resumes_from_checkpoint() {
        let tree = terminal_tree(TerminalState::Showdown);
        let first = WeightedRange {
            combos: vec![WeightedCombo {
                combo: combo("As Ah"),
                class_id: 0,
                weight: 1.0,
            }],
        };
        let second = WeightedRange {
            combos: vec![WeightedCombo {
                combo: combo("Jc Js"),
                class_id: 0,
                weight: 1.0,
            }],
        };
        let dead = mask_from_cards(&tree.nodes[0].state.board).unwrap();
        let result = solve_private_holdem_from_ranges(
            &tree,
            [&first, &second],
            dead,
            SolverAlgorithm::CfrPlus,
            4,
            99,
            777,
        )
        .unwrap();
        assert_eq!(result.private_deals, 1);
        assert_eq!(result.checkpoint.iterations, 4);
        assert_eq!(result.checkpoint.config_fingerprint, 777);
        assert!(result.average_utility[0].is_finite());

        let resumed = resume_private_holdem_from_ranges(
            &tree,
            [&first, &second],
            dead,
            &result.checkpoint,
            4,
        )
        .unwrap();
        assert_eq!(resumed.checkpoint.iterations, 8);
        assert_eq!(resumed.game_nodes, result.game_nodes);
        assert!(resume_private_holdem_from_ranges_with_config_fingerprint(
            &tree,
            [&first, &second],
            dead,
            &result.checkpoint,
            1,
            778,
        )
        .is_err());
    }

    #[test]
    fn range_expansion_is_blocker_aware_and_normalized() {
        let first = WeightedRange {
            combos: vec![
                WeightedCombo {
                    combo: combo("As Ah"),
                    class_id: 0,
                    weight: 2.0,
                },
                WeightedCombo {
                    combo: combo("Kc Kd"),
                    class_id: 0,
                    weight: 1.0,
                },
            ],
        };
        let second = WeightedRange {
            combos: vec![
                WeightedCombo {
                    combo: combo("Qs Qh"),
                    class_id: 0,
                    weight: 1.0,
                },
                WeightedCombo {
                    combo: combo("As Jc"),
                    class_id: 0,
                    weight: 3.0,
                },
            ],
        };
        let dead = mask_from_cards(&cards_from_str("2c").unwrap()).unwrap();
        let deals = private_deals_from_ranges([&first, &second], dead).unwrap();

        assert_eq!(deals.len(), 3);
        assert!((deals.iter().map(|deal| deal.probability).sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(deals
            .iter()
            .all(|deal| !deal.hands[0].conflicts(dead) && !deal.hands[1].conflicts(dead)));
        assert!(!deals
            .iter()
            .any(|deal| { deal.hands[0] == combo("As Ah") && deal.hands[1] == combo("As Jc") }));
        let kq_probability = deals
            .iter()
            .find(|deal| deal.hands == [combo("Kc Kd"), combo("Qs Qh")])
            .map(|deal| deal.probability)
            .unwrap();
        assert!((kq_probability - 1.0 / 6.0).abs() < 1e-12);
    }

    #[test]
    fn round_complete_nodes_are_rejected_without_payoff() {
        let mut state = GameState::new(
            2,
            Street::River,
            cards_from_str("2s 7d 9c Kh Qh").unwrap(),
            vec![
                PlayerState::new(0, 900).unwrap(),
                PlayerState::new(1, 900).unwrap(),
            ],
            0,
        )
        .unwrap();
        state.actor = None;
        let tree = GameTree {
            root: 0,
            nodes: vec![TreeNode {
                id: 0,
                parent: None,
                action_from_parent: None,
                state,
                children: Vec::new(),
                leaf: Some(LeafKind::RoundComplete),
                chance: None,
            }],
        };
        let payoff = HoldemChipEvPayoff::new(vec![combo("As Ah"), combo("Jc Js")], [0, 1]).unwrap();
        assert!(compile_holdem_tree(&tree, &payoff)
            .unwrap_err()
            .contains("round-complete"));
    }
}
