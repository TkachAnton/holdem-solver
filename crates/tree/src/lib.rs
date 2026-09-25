//! Generic betting-round and street-aware game tree builders.
//!
//! `build_round` builds one betting street. `build_full` additionally inserts
//! explicit public-card chance nodes between streets. Exact preflop flop
//! enumeration is intentionally opt-in because it creates 22,100 public flops
//! before private-card filtering and abstraction.

use holdem_cards::{cards_from_mask, mask_from_cards, Card};
use holdem_domain::{Action, ActionSizes, GameState, PlayerId, Street, TerminalState};

pub mod action_abstraction;
use action_abstraction::ActionAbstraction;
use holdem_cards::DeckMask;

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafKind {
    RoundComplete,
    Terminal(TerminalState),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChanceOutcome {
    pub cards: Vec<Card>,
    pub probability: f64,
}

impl ChanceOutcome {
    pub fn new(cards: Vec<Card>, probability: f64) -> Self {
        Self { cards, probability }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChanceNodeSpec {
    pub next_street: Street,
    pub outcomes: Vec<ChanceOutcome>,
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub action_from_parent: Option<Action>,
    pub state: GameState,
    pub children: Vec<NodeId>,
    pub leaf: Option<LeafKind>,
    pub chance: Option<ChanceNodeSpec>,
}

#[derive(Debug, Clone)]
pub struct GameTree {
    pub root: NodeId,
    pub nodes: Vec<TreeNode>,
}

impl GameTree {
    pub fn node(&self, id: NodeId) -> Option<&TreeNode> {
        self.nodes.get(id)
    }

    pub fn decision_nodes(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes
            .iter()
            .filter(|node| node.leaf.is_none() && node.chance.is_none())
    }

    pub fn chance_nodes(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.iter().filter(|node| node.chance.is_some())
    }

    pub fn leaf_nodes(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.iter().filter(|node| node.leaf.is_some())
    }

    pub fn history(&self, node_id: NodeId) -> Result<Vec<Action>, String> {
        let mut actions = Vec::new();
        let mut current = Some(node_id);
        while let Some(id) = current {
            let node = self
                .nodes
                .get(id)
                .ok_or_else(|| format!("unknown node id: {id}"))?;
            if let Some(action) = &node.action_from_parent {
                actions.push(action.clone());
            }
            current = node.parent;
        }
        actions.reverse();
        Ok(actions)
    }

    /// Validates structural links and the distinction between decision,
    /// chance and terminal nodes. This is intended to run after every tree
    /// build in tests and before a tree is handed to a solver.
    pub fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("tree has no nodes".to_string());
        }
        if self.root != 0 {
            return Err("tree root must have id zero".to_string());
        }

        for (index, node) in self.nodes.iter().enumerate() {
            if node.id != index {
                return Err(format!("node id/index mismatch at {index}"));
            }
            node.state.validate()?;

            if index == self.root {
                if node.parent.is_some() || node.action_from_parent.is_some() {
                    return Err("root cannot have parent or incoming action".to_string());
                }
            } else {
                let parent = node
                    .parent
                    .ok_or_else(|| format!("non-root node {index} has no parent"))?;
                let parent_node = self
                    .nodes
                    .get(parent)
                    .ok_or_else(|| format!("node {index} has invalid parent {parent}"))?;
                if !parent_node.children.contains(&index) {
                    return Err(format!("parent {parent} does not reference child {index}"));
                }
                let is_chance_child = parent_node.chance.is_some();
                if is_chance_child != node.action_from_parent.is_none() {
                    return Err(format!("invalid incoming edge type for node {index}"));
                }
            }

            for &child in &node.children {
                let child_node = self
                    .nodes
                    .get(child)
                    .ok_or_else(|| format!("node {index} references invalid child {child}"))?;
                if child_node.parent != Some(index) {
                    return Err(format!("child {child} has wrong parent"));
                }
            }

            match (&node.leaf, &node.chance) {
                (Some(_), Some(_)) => {
                    return Err(format!("node {index} cannot be leaf and chance"));
                }
                (Some(_), None) => {
                    if !node.children.is_empty() {
                        return Err(format!("leaf node {index} has children"));
                    }
                    if node.state.actor.is_some() {
                        return Err(format!("leaf node {index} still has an actor"));
                    }
                }
                (None, Some(chance)) => {
                    if node.state.actor.is_some() {
                        return Err(format!("chance node {index} has an actor"));
                    }
                    if chance.outcomes.len() != node.children.len() {
                        return Err(format!("chance node {index} outcome/child mismatch"));
                    }
                    let probability_sum: f64 = chance
                        .outcomes
                        .iter()
                        .map(|outcome| outcome.probability)
                        .sum();
                    if (probability_sum - 1.0).abs() > 1e-9 {
                        return Err(format!(
                            "chance node {index} probabilities do not sum to one"
                        ));
                    }
                }
                (None, None) => {
                    if node.state.actor.is_none() {
                        return Err(format!("non-leaf node {index} has no actor"));
                    }
                    if node.children.is_empty() {
                        return Err(format!("decision node {index} has no children"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TreeBuildConfig {
    pub action_sizes: ActionSizes,
    pub abstraction: Option<ActionAbstraction>,
    pub max_nodes: usize,
    pub max_depth: usize,
}

impl Default for TreeBuildConfig {
    fn default() -> Self {
        Self {
            action_sizes: ActionSizes::default(),
            abstraction: None,
            max_nodes: 100_000,
            max_depth: 128,
        }
    }
}

impl TreeBuildConfig {
    /// Deterministic hash of sizing and tree expansion limits.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = TREE_FNV_OFFSET;
        hash = tree_hash_value(hash, self.action_sizes.bet_to.len() as u64);
        for &value in &self.action_sizes.bet_to {
            hash = tree_hash_value(hash, value as u64);
        }
        hash = tree_hash_value(hash, self.action_sizes.raise_to.len() as u64);
        for &value in &self.action_sizes.raise_to {
            hash = tree_hash_value(hash, value as u64);
        }
        hash = tree_hash_value(hash, self.action_sizes.include_all_in as u64);
        hash = tree_hash_value(hash, self.abstraction.is_some() as u64);
        if let Some(abstraction) = &self.abstraction {
            hash = tree_hash_value(hash, abstraction.fingerprint());
        }
        hash = tree_hash_value(hash, self.max_nodes as u64);
        tree_hash_value(hash, self.max_depth as u64)
    }
}

const TREE_FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const TREE_FNV_PRIME: u64 = 0x0000_0001_0000_01b3;

fn tree_hash_value(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(TREE_FNV_PRIME);
    }
    hash
}

#[derive(Debug, Clone)]
pub struct ChanceConfig {
    pub flop: Option<Vec<ChanceOutcome>>,
    pub turn: Option<Vec<ChanceOutcome>>,
    pub river: Option<Vec<ChanceOutcome>>,
    pub enumerate_exact: bool,
    /// Известные вне игры карты (dead/hero/стартовый борд): enumerate
    /// исключает их из исходов улиц. 0 = прежнее поведение байт-в-байт.
    pub dead_cards: DeckMask,
    pub max_outcomes_per_node: usize,
}

impl Default for ChanceConfig {
    fn default() -> Self {
        Self {
            flop: None,
            turn: None,
            river: None,
            enumerate_exact: false,
            dead_cards: 0,
            max_outcomes_per_node: 10_000,
        }
    }
}

impl ChanceConfig {
    /// Deterministic hash of explicit chance branches and enumeration policy.
    pub fn fingerprint(&self) -> u64 {
        fn hash_outcomes(mut hash: u64, outcomes: &Option<Vec<ChanceOutcome>>) -> u64 {
            hash = tree_hash_value(hash, outcomes.is_some() as u64);
            if let Some(outcomes) = outcomes {
                hash = tree_hash_value(hash, outcomes.len() as u64);
                for outcome in outcomes {
                    hash = tree_hash_value(hash, outcome.cards.len() as u64);
                    for &card in &outcome.cards {
                        hash = tree_hash_value(hash, card as u64);
                    }
                    hash = tree_hash_value(hash, outcome.probability.to_bits());
                }
            }
            hash
        }

        let hash = hash_outcomes(TREE_FNV_OFFSET, &self.flop);
        let hash = hash_outcomes(hash, &self.turn);
        let hash = hash_outcomes(hash, &self.river);
        let hash = tree_hash_value(hash, self.enumerate_exact as u64);
        let hash = tree_hash_value(hash, self.dead_cards);
        tree_hash_value(hash, self.max_outcomes_per_node as u64)
    }
}

impl ChanceConfig {
    fn explicit_for(&self, next_street: Street) -> Option<&Vec<ChanceOutcome>> {
        match next_street {
            Street::Flop => self.flop.as_ref(),
            Street::Turn => self.turn.as_ref(),
            Street::River => self.river.as_ref(),
            Street::Preflop => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FullTreeBuildConfig {
    pub round: TreeBuildConfig,
    pub chance: ChanceConfig,
    pub postflop_order: Vec<PlayerId>,
}

impl FullTreeBuildConfig {
    pub fn fingerprint(&self) -> u64 {
        let mut hash = tree_hash_value(TREE_FNV_OFFSET, self.round.fingerprint());
        hash = tree_hash_value(hash, self.chance.fingerprint());
        hash = tree_hash_value(hash, self.postflop_order.len() as u64);
        for &player in &self.postflop_order {
            hash = tree_hash_value(hash, player as u64);
        }
        hash
    }
}

pub struct TreeBuilder;

impl TreeBuilder {
    pub fn build_round(initial: GameState, config: &TreeBuildConfig) -> Result<GameTree, String> {
        validate_limits(config)?;
        initial.validate()?;

        let mut tree = new_tree(initial);
        Self::expand_round(&mut tree, 0, 0, config)?;
        tree.validate()?;
        Ok(tree)
    }

    pub fn build_full(
        initial: GameState,
        config: &FullTreeBuildConfig,
    ) -> Result<GameTree, String> {
        validate_limits(&config.round)?;
        initial.validate()?;
        if config.postflop_order.is_empty() {
            return Err("postflop action order cannot be empty".to_string());
        }

        let mut tree = new_tree(initial);
        Self::expand_full(&mut tree, 0, 0, config)?;
        tree.validate()?;
        Ok(tree)
    }

    fn expand_round(
        tree: &mut GameTree,
        node_id: NodeId,
        depth: usize,
        config: &TreeBuildConfig,
    ) -> Result<(), String> {
        let state = tree_state(tree, node_id)?;
        if mark_terminal_or_round_leaf(tree, node_id, &state) {
            return Ok(());
        }
        if depth >= config.max_depth {
            return Err(format!("tree depth limit exceeded at node {node_id}"));
        }

        let action_sizes = action_sizes_for(&state, config)?;
        let actions = state.legal_actions(&action_sizes)?;
        if actions.is_empty() {
            return Err(format!("decision node {node_id} has no legal actions"));
        }

        for action in actions {
            let mut child_state = state.clone();
            child_state.apply_action(action.clone())?;
            let child_id = push_child(tree, node_id, child_state, Some(action), config.max_nodes)?;
            Self::expand_round(tree, child_id, depth + 1, config)?;
        }
        Ok(())
    }

    fn expand_full(
        tree: &mut GameTree,
        node_id: NodeId,
        depth: usize,
        config: &FullTreeBuildConfig,
    ) -> Result<(), String> {
        let state = tree_state(tree, node_id)?;

        if let Some(terminal) = state.terminal.clone() {
            tree.nodes[node_id].leaf = Some(LeafKind::Terminal(terminal));
            return Ok(());
        }

        if state.actor.is_some() {
            if depth >= config.round.max_depth {
                return Err(format!("tree depth limit exceeded at node {node_id}"));
            }
            let action_sizes = action_sizes_for(&state, &config.round)?;
            let actions = state.legal_actions(&action_sizes)?;
            if actions.is_empty() {
                return Err(format!("decision node {node_id} has no legal actions"));
            }
            for action in actions {
                let mut child_state = state.clone();
                child_state.apply_action(action.clone())?;
                let child_id = push_child(
                    tree,
                    node_id,
                    child_state,
                    Some(action),
                    config.round.max_nodes,
                )?;
                Self::expand_full(tree, child_id, depth + 1, config)?;
            }
            return Ok(());
        }

        // actor == None means that the betting round has ended. At the river
        // this becomes a showdown terminal; earlier streets create a chance
        // node and continue the tree.
        let next_street = match state.street.next() {
            Some(street) => street,
            None => {
                tree.nodes[node_id].leaf = Some(LeafKind::Terminal(TerminalState::Showdown));
                return Ok(());
            }
        };

        let outcomes = chance_outcomes(&state, next_street, &config.chance)?;
        tree.nodes[node_id].chance = Some(ChanceNodeSpec {
            next_street,
            outcomes: outcomes.clone(),
        });

        for outcome in outcomes {
            let mut child_state = state.clone();
            child_state.advance_to_next_street(&outcome.cards, &config.postflop_order)?;
            let child_id = push_child(tree, node_id, child_state, None, config.round.max_nodes)?;
            Self::expand_full(tree, child_id, depth + 1, config)?;
        }

        Ok(())
    }
}

fn validate_limits(config: &TreeBuildConfig) -> Result<(), String> {
    if config.max_nodes == 0 {
        return Err("max_nodes must be positive".to_string());
    }
    if config.max_depth == 0 {
        return Err("max_depth must be positive".to_string());
    }
    Ok(())
}

fn action_sizes_for(state: &GameState, config: &TreeBuildConfig) -> Result<ActionSizes, String> {
    match &config.abstraction {
        Some(abstraction) => abstraction.action_sizes(state),
        None => Ok(config.action_sizes.clone()),
    }
}

fn new_tree(initial: GameState) -> GameTree {
    GameTree {
        root: 0,
        nodes: vec![TreeNode {
            id: 0,
            parent: None,
            action_from_parent: None,
            state: initial,
            children: Vec::new(),
            leaf: None,
            chance: None,
        }],
    }
}

fn tree_state(tree: &GameTree, node_id: NodeId) -> Result<GameState, String> {
    tree.nodes
        .get(node_id)
        .map(|node| node.state.clone())
        .ok_or_else(|| format!("unknown node id: {node_id}"))
}

fn mark_terminal_or_round_leaf(tree: &mut GameTree, node_id: NodeId, state: &GameState) -> bool {
    if let Some(terminal) = state.terminal.clone() {
        tree.nodes[node_id].leaf = Some(LeafKind::Terminal(terminal));
        return true;
    }
    if state.actor.is_none() {
        tree.nodes[node_id].leaf = Some(LeafKind::RoundComplete);
        return true;
    }
    false
}

fn push_child(
    tree: &mut GameTree,
    parent: NodeId,
    state: GameState,
    action: Option<Action>,
    max_nodes: usize,
) -> Result<NodeId, String> {
    if tree.nodes.len() >= max_nodes {
        return Err(format!("tree node limit exceeded: {max_nodes}"));
    }
    let child_id = tree.nodes.len();
    tree.nodes.push(TreeNode {
        id: child_id,
        parent: Some(parent),
        action_from_parent: action,
        state,
        children: Vec::new(),
        leaf: None,
        chance: None,
    });
    tree.nodes[parent].children.push(child_id);
    Ok(child_id)
}

fn normalize_outcomes(outcomes: Vec<ChanceOutcome>) -> Result<Vec<ChanceOutcome>, String> {
    if outcomes.is_empty() {
        return Err("chance node has no outcomes".to_string());
    }
    let mut total = 0.0;
    for outcome in &outcomes {
        if outcome.probability <= 0.0 || !outcome.probability.is_finite() {
            return Err("chance probability must be finite and positive".to_string());
        }
        mask_from_cards(&outcome.cards).map_err(|error| error.to_string())?;
        total += outcome.probability;
    }
    if total <= 0.0 || !total.is_finite() {
        return Err("chance probabilities have invalid total".to_string());
    }

    Ok(outcomes
        .into_iter()
        .map(|mut outcome| {
            outcome.probability /= total;
            outcome
        })
        .collect())
}

fn chance_outcomes(
    state: &GameState,
    next_street: Street,
    config: &ChanceConfig,
) -> Result<Vec<ChanceOutcome>, String> {
    if let Some(explicit) = config.explicit_for(next_street) {
        if config.max_outcomes_per_node > 0 && explicit.len() > config.max_outcomes_per_node {
            return Err(format!(
                "chance outcome limit exceeded: {} > {}",
                explicit.len(),
                config.max_outcomes_per_node
            ));
        }
        return normalize_outcomes(explicit.clone());
    }

    if !config.enumerate_exact {
        return Err(format!(
            "no chance outcomes configured for transition to {next_street:?}"
        ));
    }

    let existing_mask =
        mask_from_cards(&state.board).map_err(|error| error.to_string())? | config.dead_cards;
    let remaining: Vec<Card> = cards_from_mask(!existing_mask & ((1u64 << 52) - 1));
    let needed = state.street.required_new_board_cards();
    let mut outcomes = Vec::new();

    match needed {
        1 => {
            for card in remaining {
                outcomes.push(ChanceOutcome::new(vec![card], 1.0));
                enforce_chance_limit(&outcomes, config.max_outcomes_per_node)?;
            }
        }
        3 => {
            for first in 0..remaining.len() {
                for second in (first + 1)..remaining.len() {
                    for third in (second + 1)..remaining.len() {
                        outcomes.push(ChanceOutcome::new(
                            vec![remaining[first], remaining[second], remaining[third]],
                            1.0,
                        ));
                        enforce_chance_limit(&outcomes, config.max_outcomes_per_node)?;
                    }
                }
            }
        }
        _ => return Err(format!("unsupported chance width: {needed}")),
    }

    normalize_outcomes(outcomes)
}

fn enforce_chance_limit(outcomes: &[ChanceOutcome], limit: usize) -> Result<(), String> {
    if limit > 0 && outcomes.len() > limit {
        Err(format!("chance outcome limit exceeded: {limit}"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;
    use holdem_domain::setup::build_preflop_state;
    use holdem_domain::table::{AnteMode, TableConfig};

    fn heads_up_state() -> GameState {
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
        build_preflop_state(&table).unwrap()
    }

    fn round_config() -> TreeBuildConfig {
        TreeBuildConfig {
            action_sizes: ActionSizes {
                bet_to: Vec::new(),
                raise_to: vec![2_500],
                include_all_in: false,
            },
            abstraction: None,
            max_nodes: 10_000,
            max_depth: 32,
        }
    }

    #[test]
    fn builds_a_finite_preflop_round_tree() {
        let tree = TreeBuilder::build_round(heads_up_state(), &round_config()).unwrap();
        assert!(tree.nodes.len() > 1);
        assert!(tree.leaf_nodes().count() > 0);
        assert!(tree.decision_nodes().all(|node| !node.children.is_empty()));
    }

    #[test]
    fn root_has_fold_call_and_raise_for_preflop_actor() {
        let tree = TreeBuilder::build_round(heads_up_state(), &round_config()).unwrap();
        let root = tree.node(tree.root).unwrap();
        let actions: Vec<&Action> = root
            .children
            .iter()
            .map(|&child| {
                tree.node(child)
                    .unwrap()
                    .action_from_parent
                    .as_ref()
                    .unwrap()
            })
            .collect();
        assert!(actions.contains(&&Action::Fold));
        assert!(actions.contains(&&Action::Call));
        assert!(actions.contains(&&Action::Raise { to: 2_500 }));
    }

    #[test]
    fn tree_uses_street_specific_action_abstraction() {
        let config = TreeBuildConfig {
            action_sizes: ActionSizes::default(),
            abstraction: Some(ActionAbstraction {
                preflop: action_abstraction::StreetSizing {
                    explicit_raise_to: vec![2_500],
                    ..action_abstraction::StreetSizing::default()
                },
                ..ActionAbstraction::default()
            }),
            max_nodes: 10_000,
            max_depth: 32,
        };
        let tree = TreeBuilder::build_round(heads_up_state(), &config).unwrap();
        let root = tree.node(tree.root).unwrap();
        let actions: Vec<&Action> = root
            .children
            .iter()
            .map(|&child| {
                tree.node(child)
                    .unwrap()
                    .action_from_parent
                    .as_ref()
                    .unwrap()
            })
            .collect();
        assert!(actions.contains(&&Action::Raise { to: 2_500 }));
    }

    #[test]
    fn history_is_reconstructed_from_parent_links() {
        let config = TreeBuildConfig {
            action_sizes: ActionSizes {
                bet_to: Vec::new(),
                raise_to: Vec::new(),
                include_all_in: false,
            },
            abstraction: None,
            max_nodes: 100,
            max_depth: 8,
        };
        let tree = TreeBuilder::build_round(heads_up_state(), &config).unwrap();
        let child = tree.node(tree.root).unwrap().children[0];
        let history = tree.history(child).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0], Action::Fold);
    }

    #[test]
    fn full_tree_creates_flop_turn_river_chance_nodes() {
        let config = FullTreeBuildConfig {
            round: TreeBuildConfig {
                action_sizes: ActionSizes::default(),
                abstraction: None,
                max_nodes: 10_000,
                max_depth: 64,
            },
            chance: ChanceConfig {
                flop: Some(vec![ChanceOutcome::new(
                    cards_from_str("As 7d 2c").unwrap(),
                    1.0,
                )]),
                turn: Some(vec![ChanceOutcome::new(cards_from_str("Kh").unwrap(), 1.0)]),
                river: Some(vec![ChanceOutcome::new(cards_from_str("Qc").unwrap(), 1.0)]),
                enumerate_exact: false,
                dead_cards: 0,
                max_outcomes_per_node: 100,
            },
            postflop_order: vec![1, 0],
        };

        let tree = TreeBuilder::build_full(heads_up_state(), &config).unwrap();
        assert_eq!(tree.chance_nodes().count(), 3);
        assert!(tree.nodes.iter().any(|node| {
            matches!(node.leaf, Some(LeafKind::Terminal(TerminalState::Showdown)))
                && node.state.board.len() == 5
        }));
    }

    #[test]
    fn explicit_chance_probabilities_are_normalized() {
        let state = heads_up_state();
        let config = FullTreeBuildConfig {
            round: TreeBuildConfig {
                action_sizes: ActionSizes::default(),
                abstraction: None,
                max_nodes: 10_000,
                max_depth: 32,
            },
            chance: ChanceConfig {
                flop: Some(vec![
                    ChanceOutcome::new(cards_from_str("As 7d 2c").unwrap(), 2.0),
                    ChanceOutcome::new(cards_from_str("Kh Qh Jc").unwrap(), 1.0),
                ]),
                turn: Some(vec![ChanceOutcome::new(cards_from_str("Td").unwrap(), 1.0)]),
                river: Some(vec![ChanceOutcome::new(cards_from_str("9s").unwrap(), 1.0)]),
                enumerate_exact: false,
                dead_cards: 0,
                max_outcomes_per_node: 100,
            },
            postflop_order: vec![1, 0],
        };
        let tree = TreeBuilder::build_full(state, &config).unwrap();
        let chance = tree.chance_nodes().next().unwrap();
        let total: f64 = chance
            .chance
            .as_ref()
            .unwrap()
            .outcomes
            .iter()
            .map(|outcome| outcome.probability)
            .sum();
        assert!((total - 1.0).abs() < 1e-12);
    }

    #[test]
    fn build_config_fingerprint_changes_with_chance_policy() {
        let base = FullTreeBuildConfig {
            round: TreeBuildConfig::default(),
            chance: ChanceConfig::default(),
            postflop_order: vec![1, 0],
        };
        let changed = FullTreeBuildConfig {
            chance: ChanceConfig {
                enumerate_exact: true,
                ..ChanceConfig::default()
            },
            ..base.clone()
        };
        assert_eq!(base.fingerprint(), base.clone().fingerprint());
        assert_ne!(base.fingerprint(), changed.fingerprint());
        assert_eq!(base.round.fingerprint(), changed.round.fingerprint());
    }
}
