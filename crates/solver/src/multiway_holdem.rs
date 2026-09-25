//! Multiway Hold'em private-card compiler and ChipEV payoff adapter.
//!
//! This is the next layer above `multiway::MultiwayMccfrSolver`: it preserves
//! exact blocker-conditioned private deals, compiles a public `GameTree` into
//! dynamic-utility multiway solver nodes, and settles fold/showdown terminals
//! for 3-8 seats. Exact Cartesian expansion is intentionally guarded by an
//! explicit deal limit; production-sized ranges use the deterministic rejection
//! sampler exposed by this module and the public-tree batch layer.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use holdem_cards::{mask_from_cards, DeckMask};
use holdem_domain::{Action, PlayerId, TerminalState};
use holdem_ranges::{Combo, WeightedRange};
use holdem_settlement::exact_chip_ev_showdown;
use holdem_tree::{GameTree, LeafKind, NodeId as TreeNodeId, TreeNode};

use crate::{
    InfoSetId, MultiwayGameNode, MultiwayMccfrSolver, MultiwayNodeId, MultiwaySolverCheckpoint,
    MultiwayStaticGame, XorShift64,
};

#[derive(Debug, Clone, PartialEq)]
pub struct MultiwayPrivateDeal {
    pub probability: f64,
    pub hands: Vec<Combo>,
}

/// Expands exact weighted ranges into legal 3-8 player private deals.
///
/// The product is conditioned on all hands being pairwise legal and on
/// `dead_cards`. `max_deals` is mandatory as a safety guard because the exact
/// Cartesian product grows rapidly for realistic 8-max ranges.
pub fn multiway_private_deals_from_ranges(
    ranges: &[&WeightedRange],
    dead_cards: DeckMask,
    max_deals: usize,
) -> Result<Vec<MultiwayPrivateDeal>, String> {
    if !(3..=8).contains(&ranges.len()) {
        return Err(format!(
            "multiway private deals require 3-8 ranges, got {}",
            ranges.len()
        ));
    }
    if max_deals == 0 {
        return Err("max_deals must be positive".to_string());
    }

    let mut candidates = Vec::with_capacity(ranges.len());
    for (player, range) in ranges.iter().enumerate() {
        let mut by_combo = BTreeMap::<Combo, f64>::new();
        for entry in &range.combos {
            if !entry.weight.is_finite() || entry.weight < 0.0 {
                return Err(format!("range {player} contains an invalid combo weight"));
            }
            if entry.weight == 0.0 || entry.combo.conflicts(dead_cards) {
                continue;
            }
            *by_combo.entry(entry.combo).or_insert(0.0) += entry.weight;
        }
        let entries: Vec<(Combo, f64)> = by_combo
            .into_iter()
            .filter(|(_, weight)| *weight > 0.0 && weight.is_finite())
            .collect();
        if entries.is_empty() {
            return Err(format!("range {player} has no legal weighted combos"));
        }
        candidates.push(entries);
    }

    let mut raw = Vec::<(Vec<Combo>, f64)>::new();
    enumerate_multiway_deals(
        &candidates,
        0,
        dead_cards,
        &mut Vec::with_capacity(ranges.len()),
        1.0,
        max_deals,
        &mut raw,
    )?;

    let total: f64 = raw.iter().map(|(_, weight)| *weight).sum();
    if total <= 0.0 || !total.is_finite() {
        return Err("ranges have no legal multiway private-card deals".to_string());
    }
    Ok(raw
        .into_iter()
        .map(|(hands, weight)| MultiwayPrivateDeal {
            probability: weight / total,
            hands,
        })
        .collect())
}

fn enumerate_multiway_deals(
    candidates: &[Vec<(Combo, f64)>],
    player: usize,
    used_cards: DeckMask,
    current: &mut Vec<Combo>,
    weight: f64,
    max_deals: usize,
    output: &mut Vec<(Vec<Combo>, f64)>,
) -> Result<(), String> {
    if player == candidates.len() {
        if output.len() >= max_deals {
            return Err(format!("multiway private-deal limit exceeded: {max_deals}"));
        }
        output.push((current.clone(), weight));
        return Ok(());
    }

    for &(combo, combo_weight) in &candidates[player] {
        if combo.mask() & used_cards != 0 {
            continue;
        }
        let next_weight = weight * combo_weight;
        if !next_weight.is_finite() || next_weight < 0.0 {
            return Err("multiway range product has an invalid weight".to_string());
        }
        current.push(combo);
        enumerate_multiway_deals(
            candidates,
            player + 1,
            used_cards | combo.mask(),
            current,
            next_weight,
            max_deals,
            output,
        )?;
        current.pop();
    }
    Ok(())
}

/// One exact private-card profile returned by the rejection sampler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiwayPrivateDealSample {
    pub hands: Vec<Combo>,
    /// Number of independent proposals used before this legal profile was
    /// accepted. This is a useful blocker/acceptance diagnostic.
    pub attempts: u64,
}

/// Samples the blocker-conditioned product of 3-8 weighted ranges.
///
/// Each player's combo is first drawn independently according to its weighted
/// range after `dead_cards` filtering. Profiles with cross-player blockers are
/// rejected and resampled. Consequently, accepted profiles follow the exact
/// product distribution conditioned on pairwise legal hands, without building
/// the full Cartesian product in memory. The caller should use the returned
/// profile as a sampled chance outcome, not as an enumerated deal with a
/// fabricated probability.
#[derive(Debug, Clone)]
pub struct MultiwayPrivateDealSampler {
    candidates: Arc<Vec<Vec<(Combo, f64)>>>,
    probabilities: Arc<Vec<Vec<f64>>>,
    dead_cards: DeckMask,
    range_fingerprint: u64,
    rng: XorShift64,
    total_attempts: u64,
}

fn sampler_fingerprint(dead_cards: DeckMask, candidates: &[Vec<(Combo, f64)>]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0001_0000_01b3;
    fn update(mut hash: u64, value: u64) -> u64 {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }

    let mut hash = update(OFFSET, dead_cards);
    hash = update(hash, candidates.len() as u64);
    for entries in candidates {
        hash = update(hash, entries.len() as u64);
        for &(combo, weight) in entries {
            hash = update(hash, combo.cards[0] as u64);
            hash = update(hash, combo.cards[1] as u64);
            hash = update(hash, weight.to_bits());
        }
    }
    hash
}

impl MultiwayPrivateDealSampler {
    pub fn new(ranges: &[&WeightedRange], dead_cards: DeckMask, seed: u64) -> Result<Self, String> {
        if !(3..=8).contains(&ranges.len()) {
            return Err(format!(
                "multiway private sampler requires 3-8 ranges, got {}",
                ranges.len()
            ));
        }

        let mut candidates = Vec::with_capacity(ranges.len());
        let mut probabilities = Vec::with_capacity(ranges.len());
        for (player, range) in ranges.iter().enumerate() {
            let mut by_combo = BTreeMap::<Combo, f64>::new();
            for entry in &range.combos {
                if !entry.weight.is_finite() || entry.weight < 0.0 {
                    return Err(format!("range {player} contains an invalid combo weight"));
                }
                if entry.weight == 0.0 || entry.combo.conflicts(dead_cards) {
                    continue;
                }
                *by_combo.entry(entry.combo).or_insert(0.0) += entry.weight;
            }
            let entries: Vec<(Combo, f64)> = by_combo
                .into_iter()
                .filter(|(_, weight)| *weight > 0.0 && weight.is_finite())
                .collect();
            if entries.is_empty() {
                return Err(format!("range {player} has no legal weighted combos"));
            }
            let total: f64 = entries.iter().map(|(_, weight)| *weight).sum();
            if total <= 0.0 || !total.is_finite() {
                return Err(format!("range {player} has invalid total weight"));
            }
            probabilities.push(entries.iter().map(|(_, weight)| *weight / total).collect());
            candidates.push(entries);
        }

        let range_fingerprint = sampler_fingerprint(dead_cards, &candidates);
        Ok(Self {
            candidates: Arc::new(candidates),
            probabilities: Arc::new(probabilities),
            dead_cards,
            range_fingerprint,
            rng: XorShift64::new(seed),
            total_attempts: 0,
        })
    }

    /// T4.3/D-022: взвешенный выбор руки одного игрока по нормированным
    /// весам его диапазона (для выбора рук дрилла).
    pub fn sample_hand_for_player(&mut self, player: usize) -> Result<Combo, String> {
        if player >= self.candidates.len() {
            return Err(format!(
                "sampler player {player} is outside the range vector"
            ));
        }
        let index = self.rng.sample_index(&self.probabilities[player]);
        Ok(self.candidates[player][index].0)
    }

    /// T4.3/D-022: дил с фиксированной рукой одного игрока; остальные
    /// сэмплируются по своим вероятностям; конфликт — rejection (как в
    /// `sample`, но фиксированная рука не перевыбирается).
    pub fn sample_conditioned(
        &mut self,
        player: usize,
        hand: Combo,
        max_attempts: usize,
    ) -> Result<MultiwayPrivateDealSample, String> {
        if max_attempts == 0 {
            return Err("max_attempts must be positive".to_string());
        }
        if player >= self.candidates.len() {
            return Err(format!(
                "sampler player {player} is outside the range vector"
            ));
        }
        if !self.candidates[player]
            .iter()
            .any(|(combo, _)| *combo == hand)
        {
            return Err(format!(
                "conditioned hand is not a legal candidate for player {player}"
            ));
        }
        for attempt in 1..=max_attempts {
            self.total_attempts += 1;
            let mut used_cards = self.dead_cards;
            let mut hands = Vec::with_capacity(self.candidates.len());
            let mut legal = true;
            for current in 0..self.candidates.len() {
                if current == player {
                    if hand.mask() & used_cards != 0 {
                        legal = false;
                        break;
                    }
                    hands.push(hand);
                    used_cards |= hand.mask();
                    continue;
                }
                let index = self.rng.sample_index(&self.probabilities[current]);
                let combo = self.candidates[current][index].0;
                if combo.mask() & used_cards != 0 {
                    legal = false;
                    break;
                }
                used_cards |= combo.mask();
                hands.push(combo);
            }
            if legal {
                return Ok(MultiwayPrivateDealSample {
                    hands,
                    attempts: attempt as u64,
                });
            }
        }
        Err(format!(
            "could not sample a conditioned multiway private deal within {max_attempts} attempts"
        ))
    }

    pub fn total_attempts(&self) -> u64 {
        self.total_attempts
    }

    pub fn rng_state(&self) -> u64 {
        self.rng.state
    }

    pub fn dead_cards(&self) -> DeckMask {
        self.dead_cards
    }

    pub fn range_fingerprint(&self) -> u64 {
        self.range_fingerprint
    }

    pub fn from_state(
        ranges: &[&WeightedRange],
        dead_cards: DeckMask,
        rng_state: u64,
        total_attempts: u64,
    ) -> Result<Self, String> {
        if rng_state == 0 {
            return Err("multiway private sampler RNG state cannot be zero".to_string());
        }
        let mut sampler = Self::new(ranges, dead_cards, 1)?;
        sampler.rng = XorShift64 { state: rng_state };
        sampler.total_attempts = total_attempts;
        Ok(sampler)
    }

    pub(crate) fn fork_with_state(
        &self,
        rng_state: u64,
        total_attempts: u64,
    ) -> Result<Self, String> {
        if rng_state == 0 {
            return Err("multiway private sampler RNG state cannot be zero".to_string());
        }
        let mut sampler = self.clone();
        sampler.rng = XorShift64 { state: rng_state };
        sampler.total_attempts = total_attempts;
        Ok(sampler)
    }

    pub fn sample(&mut self, max_attempts: usize) -> Result<MultiwayPrivateDealSample, String> {
        if max_attempts == 0 {
            return Err("max_attempts must be positive".to_string());
        }
        for attempt in 1..=max_attempts {
            self.total_attempts += 1;
            let mut used_cards = self.dead_cards;
            let mut hands = Vec::with_capacity(self.candidates.len());
            let mut legal = true;
            for player in 0..self.candidates.len() {
                let index = self.rng.sample_index(&self.probabilities[player]);
                let combo = self.candidates[player][index].0;
                if combo.mask() & used_cards != 0 {
                    legal = false;
                    break;
                }
                used_cards |= combo.mask();
                hands.push(combo);
            }
            if legal {
                return Ok(MultiwayPrivateDealSample {
                    hands,
                    attempts: attempt as u64,
                });
            }
        }
        Err(format!(
            "could not sample a legal multiway private deal within {max_attempts} attempts"
        ))
    }
}

pub fn sample_multiway_private_deal(
    ranges: &[&WeightedRange],
    dead_cards: DeckMask,
    seed: u64,
    max_attempts: usize,
) -> Result<MultiwayPrivateDealSample, String> {
    MultiwayPrivateDealSampler::new(ranges, dead_cards, seed)?.sample(max_attempts)
}

pub trait MultiwayTerminalPayoff {
    fn utility(&self, node: &TreeNode) -> Result<Vec<f64>, String>;
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemChipEvPayoff {
    hands: Vec<Combo>,
}

impl MultiwayHoldemChipEvPayoff {
    pub fn new(hands: Vec<Combo>) -> Result<Self, String> {
        if !(3..=8).contains(&hands.len()) {
            return Err(format!(
                "multiway Hold'em payoff requires 3-8 hands, got {}",
                hands.len()
            ));
        }
        let mut used_cards = 0;
        for hand in &hands {
            if hand.mask() & used_cards != 0 {
                return Err("multiway private hands contain duplicate cards".to_string());
            }
            used_cards |= hand.mask();
        }
        Ok(Self { hands })
    }

    pub fn hands(&self) -> &[Combo] {
        &self.hands
    }

    fn validate_state(&self, node: &TreeNode) -> Result<(), String> {
        if node.state.table_size != self.hands.len() || node.state.players.len() != self.hands.len()
        {
            return Err(format!(
                "tree state has {} players but payoff has {} hands",
                node.state.players.len(),
                self.hands.len()
            ));
        }
        Ok(())
    }
}

impl MultiwayTerminalPayoff for MultiwayHoldemChipEvPayoff {
    fn utility(&self, node: &TreeNode) -> Result<Vec<f64>, String> {
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
                if *winner >= self.hands.len() {
                    return Err(format!("fold winner is out of range: {winner}"));
                }
                let mut utility = vec![0.0; self.hands.len()];
                for (player, value) in utility.iter_mut().enumerate() {
                    let payout = if player == *winner {
                        node.state.pot as f64
                    } else {
                        0.0
                    };
                    *value = payout - node.state.players[player].committed_total as f64;
                }
                Ok(utility)
            }
            TerminalState::Showdown => {
                let settlement = exact_chip_ev_showdown(&node.state, &self.hands)?;
                Ok(settlement.net_ev)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct MultiwayCompiledHoldemGame {
    game: MultiwayStaticGame,
    actions: Vec<Vec<Action>>,
    tree_node_ids: Vec<Option<TreeNodeId>>,
    private_hands: Vec<Option<Vec<Combo>>>,
}

impl MultiwayCompiledHoldemGame {
    pub fn game(&self) -> &MultiwayStaticGame {
        &self.game
    }

    pub fn into_game(self) -> MultiwayStaticGame {
        self.game
    }

    pub fn actions_at(&self, node_id: MultiwayNodeId) -> Option<&[Action]> {
        self.actions.get(node_id).map(Vec::as_slice)
    }

    pub fn action(&self, node_id: MultiwayNodeId, action_index: usize) -> Option<&Action> {
        self.actions_at(node_id)?.get(action_index)
    }

    pub fn tree_node_id(&self, solver_node_id: MultiwayNodeId) -> Option<TreeNodeId> {
        self.tree_node_ids.get(solver_node_id).copied().flatten()
    }

    pub fn private_hands_at(&self, solver_node_id: MultiwayNodeId) -> Option<&[Combo]> {
        self.private_hands.get(solver_node_id)?.as_deref()
    }

    pub fn node_count(&self) -> usize {
        self.game.nodes().len()
    }

    pub fn private_deal_count(&self) -> usize {
        match self.game.node(self.game.root()) {
            Some(MultiwayGameNode::Chance { outcomes }) => outcomes.len(),
            _ => 0,
        }
    }
}

/// Compiles a validated multiway public tree for a finite private-deal list.
///
/// Information sets are shared by `(public node, acting player, own exact
/// Combo)` and never include opponents' private cards. Public-card outcomes
/// conflicting with a deal are removed and renormalized independently per
/// deal, exactly as in the heads-up compiler.
pub fn compile_multiway_holdem_tree(
    tree: &GameTree,
    deals: &[MultiwayPrivateDeal],
) -> Result<MultiwayCompiledHoldemGame, String> {
    tree.validate()?;
    validate_reachable_acyclic_tree(tree)?;
    if deals.is_empty() {
        return Err("multiway private deal list cannot be empty".to_string());
    }

    let player_count = tree
        .node(tree.root)
        .ok_or_else(|| "tree root is missing".to_string())?
        .state
        .table_size;
    if !(3..=8).contains(&player_count) {
        return Err(format!(
            "multiway Hold'em compiler requires 3-8 seats, got {player_count}"
        ));
    }

    let mut total_probability = 0.0;
    for deal in deals {
        if deal.hands.len() != player_count {
            return Err(format!(
                "deal has {} hands, expected {player_count}",
                deal.hands.len()
            ));
        }
        if deal.probability <= 0.0 || !deal.probability.is_finite() {
            return Err("private deal probability must be finite and positive".to_string());
        }
        MultiwayHoldemChipEvPayoff::new(deal.hands.clone())?;
        total_probability += deal.probability;
    }
    if total_probability <= 0.0 || !total_probability.is_finite() {
        return Err("private deal probabilities have invalid total".to_string());
    }

    let mut nodes = vec![MultiwayGameNode::Chance {
        outcomes: Vec::new(),
    }];
    let mut actions = vec![Vec::new()];
    let mut tree_node_ids = vec![None];
    let mut private_hands = vec![None];
    let mut infoset_ids = HashMap::new();
    let mut outcomes = Vec::with_capacity(deals.len());

    for deal in deals {
        let payoff = MultiwayHoldemChipEvPayoff::new(deal.hands.clone())?;
        let branch = append_multiway_node(
            tree,
            tree.root,
            &deal.hands,
            &payoff,
            &mut nodes,
            &mut actions,
            &mut tree_node_ids,
            &mut private_hands,
            &mut infoset_ids,
        )?;
        outcomes.push((deal.probability / total_probability, branch));
    }
    nodes[0] = MultiwayGameNode::Chance { outcomes };

    let game = MultiwayStaticGame::new(player_count, 0, nodes)?;
    Ok(MultiwayCompiledHoldemGame {
        game,
        actions,
        tree_node_ids,
        private_hands,
    })
}

fn append_multiway_node(
    tree: &GameTree,
    tree_node_id: TreeNodeId,
    hands: &[Combo],
    payoff: &MultiwayHoldemChipEvPayoff,
    nodes: &mut Vec<MultiwayGameNode>,
    actions: &mut Vec<Vec<Action>>,
    tree_node_ids: &mut Vec<Option<TreeNodeId>>,
    private_hands: &mut Vec<Option<Vec<Combo>>>,
    infoset_ids: &mut HashMap<(TreeNodeId, PlayerId, Combo), InfoSetId>,
) -> Result<MultiwayNodeId, String> {
    let tree_node = tree
        .node(tree_node_id)
        .ok_or_else(|| format!("unknown public tree node {tree_node_id}"))?;
    let hands_mask = hands.iter().fold(0, |mask, hand| mask | hand.mask());
    let board_mask = mask_from_cards(&tree_node.state.board).map_err(|error| error.to_string())?;
    if hands_mask & board_mask != 0 {
        return Err(format!(
            "private deal conflicts with public board at tree node {}",
            tree_node.id
        ));
    }

    let solver_node_id = nodes.len();
    nodes.push(MultiwayGameNode::Terminal {
        utility: vec![0.0; hands.len()],
    });
    actions.push(Vec::new());
    tree_node_ids.push(Some(tree_node_id));
    private_hands.push(Some(hands.to_vec()));

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
                let child_id = append_multiway_node(
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
        MultiwayGameNode::Chance { outcomes }
    } else if let Some(leaf) = &tree_node.leaf {
        match leaf {
            LeafKind::Terminal(_) => MultiwayGameNode::Terminal {
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
        if player >= hands.len() {
            return Err(format!(
                "decision node {} has player {player} outside private deal",
                tree_node.id
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
            child_ids.push(append_multiway_node(
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
        MultiwayGameNode::Decision {
            player,
            infoset,
            children: child_ids,
        }
    };

    nodes[solver_node_id] = compiled;
    Ok(solver_node_id)
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

#[derive(Debug, Clone)]
pub struct MultiwayHoldemSolveResult {
    pub checkpoint: MultiwaySolverCheckpoint,
    pub average_utility: Vec<f64>,
    pub game_nodes: usize,
    pub private_deals: usize,
}

pub fn run_compiled_multiway_holdem_game(
    compiled: &MultiwayCompiledHoldemGame,
    iterations: u64,
    seed: u64,
    config_fingerprint: u64,
) -> Result<MultiwayHoldemSolveResult, String> {
    let mut solver = MultiwayMccfrSolver::new(compiled.game().clone(), seed, config_fingerprint)?;
    solver.run(iterations)?;
    let average_utility = solver.evaluate_average_strategy()?;
    Ok(MultiwayHoldemSolveResult {
        checkpoint: solver.checkpoint(),
        average_utility,
        game_nodes: compiled.node_count(),
        private_deals: compiled.private_deal_count(),
    })
}

pub fn resume_compiled_multiway_holdem_game(
    compiled: &MultiwayCompiledHoldemGame,
    checkpoint: &MultiwaySolverCheckpoint,
    additional_iterations: u64,
    config_fingerprint: u64,
) -> Result<MultiwayHoldemSolveResult, String> {
    let mut solver = MultiwayMccfrSolver::from_checkpoint(
        compiled.game().clone(),
        checkpoint,
        config_fingerprint,
    )?;
    solver.run(additional_iterations)?;
    let average_utility = solver.evaluate_average_strategy()?;
    Ok(MultiwayHoldemSolveResult {
        checkpoint: solver.checkpoint(),
        average_utility,
        game_nodes: compiled.node_count(),
        private_deals: compiled.private_deal_count(),
    })
}

pub fn solve_multiway_holdem_from_ranges(
    tree: &GameTree,
    ranges: &[&WeightedRange],
    dead_cards: DeckMask,
    max_deals: usize,
    iterations: u64,
    seed: u64,
    config_fingerprint: u64,
) -> Result<MultiwayHoldemSolveResult, String> {
    let deals = multiway_private_deals_from_ranges(ranges, dead_cards, max_deals)?;
    let compiled = compile_multiway_holdem_tree(tree, &deals)?;
    run_compiled_multiway_holdem_game(&compiled, iterations, seed, config_fingerprint)
}

pub fn resume_multiway_holdem_from_ranges(
    tree: &GameTree,
    ranges: &[&WeightedRange],
    dead_cards: DeckMask,
    max_deals: usize,
    checkpoint: &MultiwaySolverCheckpoint,
    additional_iterations: u64,
    config_fingerprint: u64,
) -> Result<MultiwayHoldemSolveResult, String> {
    let deals = multiway_private_deals_from_ranges(ranges, dead_cards, max_deals)?;
    let compiled = compile_multiway_holdem_tree(tree, &deals)?;
    resume_compiled_multiway_holdem_game(
        &compiled,
        checkpoint,
        additional_iterations,
        config_fingerprint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;
    use holdem_domain::{GameState, PlayerState, Street};

    fn combo(text: &str) -> Combo {
        let cards = cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    fn range(combos: &[&str]) -> WeightedRange {
        WeightedRange {
            combos: combos
                .iter()
                .map(|text| {
                    let combo = combo(text);
                    holdem_ranges::WeightedCombo {
                        combo,
                        class_id: combo.class_id(),
                        weight: 1.0,
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn multiway_range_expansion_is_blocker_aware_and_normalized() {
        let first = range(&["As Ah", "Kc Kd"]);
        let second = range(&["Qs Qh", "As Jc"]);
        let third = range(&["Tc Td", "9c 9d"]);
        let deals = multiway_private_deals_from_ranges(
            &[&first, &second, &third],
            mask_from_cards(&[]).unwrap(),
            32,
        )
        .unwrap();
        assert_eq!(deals.len(), 6);
        assert!((deals.iter().map(|deal| deal.probability).sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(deals.iter().all(|deal| deal.hands.len() == 3));
        assert!(!deals
            .iter()
            .any(|deal| { deal.hands[0] == combo("As Ah") && deal.hands[1] == combo("As Jc") }));
    }

    #[test]
    fn multiway_rejection_sampler_is_deterministic_and_blocker_safe() {
        let first = range(&["As Ah", "Kc Kd"]);
        let second = range(&["Qs Qh", "As Jc"]);
        let third = range(&["Tc Td", "9c 9d"]);
        let dead = mask_from_cards(&[]).unwrap();
        let mut left =
            MultiwayPrivateDealSampler::new(&[&first, &second, &third], dead, 99).unwrap();
        let mut right =
            MultiwayPrivateDealSampler::new(&[&first, &second, &third], dead, 99).unwrap();

        for _ in 0..32 {
            let left_sample = left.sample(100).unwrap();
            let right_sample = right.sample(100).unwrap();
            assert_eq!(left_sample, right_sample);
            assert_eq!(left_sample.hands.len(), 3);
            let mut used = 0;
            for hand in left_sample.hands {
                assert_eq!(hand.mask() & used, 0);
                used |= hand.mask();
            }
        }
        assert!(left.total_attempts() >= 32);
    }

    #[test]
    fn multiway_payoff_returns_dynamic_chip_ev_vector() {
        let mut players = vec![
            PlayerState::new(0, 900).unwrap(),
            PlayerState::new(1, 900).unwrap(),
            PlayerState::new(2, 900).unwrap(),
        ];
        for player in &mut players {
            player.committed_total = 100;
            player.committed_street = 100;
        }
        let mut state = GameState::new(
            3,
            Street::Flop,
            cards_from_str("2s 7d 9c").unwrap(),
            players,
            0,
        )
        .unwrap();
        state.terminal = Some(TerminalState::Showdown);
        let tree_node = TreeNode {
            id: 0,
            parent: None,
            action_from_parent: None,
            state,
            children: Vec::new(),
            leaf: Some(LeafKind::Terminal(TerminalState::Showdown)),
            chance: None,
        };
        let payoff =
            MultiwayHoldemChipEvPayoff::new(vec![combo("As Ah"), combo("Kc Kh"), combo("Qd Qh")])
                .unwrap();
        let utility = payoff.utility(&tree_node).unwrap();
        assert_eq!(utility.len(), 3);
        assert!((utility.iter().sum::<f64>()).abs() < 1e-9);
        assert!(utility[0] > utility[1]);
    }
}
