//! Multiway external-sampling MCCFR foundation.
//!
//! This module deliberately lives beside the existing two-player `StaticGame`
//! engine. The two-player API remains backwards compatible, while this layer
//! provides dynamic utility vectors, N-player information sets, deterministic
//! checkpoints, and sampling diagnostics needed by 3-8 player Hold'em.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{InfoSetData, InfoSetId, XorShift64};

pub type MultiwayNodeId = usize;

#[derive(Debug, Clone)]
pub enum MultiwayGameNode {
    Decision {
        player: usize,
        infoset: InfoSetId,
        children: Vec<MultiwayNodeId>,
    },
    Chance {
        outcomes: Vec<(f64, MultiwayNodeId)>,
    },
    Terminal {
        utility: Vec<f64>,
    },
}

#[derive(Debug, Clone)]
pub struct MultiwayStaticGame {
    player_count: usize,
    root: MultiwayNodeId,
    nodes: Vec<MultiwayGameNode>,
}

impl MultiwayStaticGame {
    pub fn new(
        player_count: usize,
        root: MultiwayNodeId,
        nodes: Vec<MultiwayGameNode>,
    ) -> Result<Self, String> {
        let game = Self {
            player_count,
            root,
            nodes,
        };
        game.validate()?;
        Ok(game)
    }

    pub fn player_count(&self) -> usize {
        self.player_count
    }

    pub fn root(&self) -> MultiwayNodeId {
        self.root
    }

    pub fn node(&self, id: MultiwayNodeId) -> Option<&MultiwayGameNode> {
        self.nodes.get(id)
    }

    pub fn nodes(&self) -> &[MultiwayGameNode] {
        &self.nodes
    }

    pub fn fingerprint(&self) -> u64 {
        multiway_game_fingerprint(self)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.player_count < 3 {
            return Err(format!(
                "multiway game requires at least three players, got {}",
                self.player_count
            ));
        }
        if self.nodes.is_empty() {
            return Err("multiway game has no nodes".to_string());
        }
        if self.root >= self.nodes.len() {
            return Err("multiway game root is out of range".to_string());
        }

        for (id, node) in self.nodes.iter().enumerate() {
            match node {
                MultiwayGameNode::Decision {
                    player, children, ..
                } => {
                    if *player >= self.player_count {
                        return Err(format!(
                            "decision node {id} has player {player} outside {} players",
                            self.player_count
                        ));
                    }
                    if children.is_empty() {
                        return Err(format!("decision node {id} has no actions"));
                    }
                    validate_child_ids(id, children, self.nodes.len())?;
                }
                MultiwayGameNode::Chance { outcomes } => {
                    if outcomes.is_empty() {
                        return Err(format!("chance node {id} has no outcomes"));
                    }
                    let mut total = 0.0;
                    for &(probability, child) in outcomes {
                        if probability <= 0.0 || !probability.is_finite() {
                            return Err(format!(
                                "chance node {id} has invalid probability {probability}"
                            ));
                        }
                        if child >= self.nodes.len() {
                            return Err(format!(
                                "chance node {id} references invalid child {child}"
                            ));
                        }
                        total += probability;
                    }
                    if (total - 1.0).abs() > 1e-9 {
                        return Err(format!("chance node {id} probabilities do not sum to one"));
                    }
                }
                MultiwayGameNode::Terminal { utility } => {
                    if utility.len() != self.player_count {
                        return Err(format!(
                            "terminal node {id} has utility length {}, expected {}",
                            utility.len(),
                            self.player_count
                        ));
                    }
                    if !utility.iter().all(|value| value.is_finite()) {
                        return Err(format!("terminal node {id} has non-finite utility"));
                    }
                }
            }
        }

        let mut marks = vec![0u8; self.nodes.len()];
        validate_reachable_node(self, self.root, &mut marks)?;
        if marks.iter().any(|mark| *mark == 0) {
            return Err("multiway game contains unreachable nodes".to_string());
        }
        Ok(())
    }
}

fn validate_child_ids(
    node_id: MultiwayNodeId,
    children: &[MultiwayNodeId],
    node_count: usize,
) -> Result<(), String> {
    for &child in children {
        if child >= node_count {
            return Err(format!("node {node_id} references invalid child {child}"));
        }
    }
    Ok(())
}

fn validate_reachable_node(
    game: &MultiwayStaticGame,
    node_id: MultiwayNodeId,
    marks: &mut [u8],
) -> Result<(), String> {
    match marks[node_id] {
        1 => return Err(format!("multiway game contains a cycle at node {node_id}")),
        2 => return Ok(()),
        _ => {}
    }
    marks[node_id] = 1;
    match game
        .node(node_id)
        .ok_or_else(|| format!("unknown multiway node {node_id}"))?
    {
        MultiwayGameNode::Decision { children, .. } => {
            for &child in children {
                validate_reachable_node(game, child, marks)?;
            }
        }
        MultiwayGameNode::Chance { outcomes } => {
            for &(_, child) in outcomes {
                validate_reachable_node(game, child, marks)?;
            }
        }
        MultiwayGameNode::Terminal { .. } => {}
    }
    marks[node_id] = 2;
    Ok(())
}

fn multiway_game_fingerprint(game: &MultiwayStaticGame) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0001_0000_01b3;

    fn update(mut hash: u64, value: u64) -> u64 {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }

    let mut hash = update(OFFSET, game.player_count as u64);
    hash = update(hash, game.root as u64);
    hash = update(hash, game.nodes.len() as u64);
    for node in &game.nodes {
        match node {
            MultiwayGameNode::Decision {
                player,
                infoset,
                children,
            } => {
                hash = update(hash, 1);
                hash = update(hash, *player as u64);
                hash = update(hash, *infoset);
                hash = update(hash, children.len() as u64);
                for &child in children {
                    hash = update(hash, child as u64);
                }
            }
            MultiwayGameNode::Chance { outcomes } => {
                hash = update(hash, 2);
                hash = update(hash, outcomes.len() as u64);
                for &(probability, child) in outcomes {
                    hash = update(hash, probability.to_bits());
                    hash = update(hash, child as u64);
                }
            }
            MultiwayGameNode::Terminal { utility } => {
                hash = update(hash, 3);
                hash = update(hash, utility.len() as u64);
                for value in utility {
                    hash = update(hash, value.to_bits());
                }
            }
        }
    }
    hash
}

const MULTIWAY_CHECKPOINT_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultiwayAlgorithm {
    ExternalSamplingMccfr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayInfoSetCheckpoint {
    pub player: usize,
    pub infoset: InfoSetId,
    pub regret_sum: Vec<f64>,
    pub strategy_sum: Vec<f64>,
    pub visits: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiwaySamplingMetrics {
    pub traverser_updates: u64,
    pub sampled_chance_nodes: u64,
    pub sampled_opponent_actions: u64,
    pub infoset_visits: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwaySolverCheckpoint {
    pub format_version: u32,
    pub algorithm: MultiwayAlgorithm,
    pub player_count: usize,
    pub game_fingerprint: u64,
    #[serde(default)]
    pub config_fingerprint: u64,
    pub iterations: u64,
    pub infosets: Vec<MultiwayInfoSetCheckpoint>,
    pub rng_state: Option<u64>,
    #[serde(default)]
    pub metrics: MultiwaySamplingMetrics,
}

impl MultiwaySolverCheckpoint {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != MULTIWAY_CHECKPOINT_FORMAT_VERSION {
            return Err(format!(
                "unsupported multiway checkpoint format version: {}",
                self.format_version
            ));
        }
        if self.player_count < 3 {
            return Err("multiway checkpoint must contain at least three players".to_string());
        }
        if self.algorithm != MultiwayAlgorithm::ExternalSamplingMccfr {
            return Err("unsupported multiway algorithm".to_string());
        }
        if self.rng_state.unwrap_or(0) == 0 {
            return Err("multiway MCCFR checkpoint is missing RNG state".to_string());
        }

        let mut keys = HashSet::new();
        for entry in &self.infosets {
            if entry.player >= self.player_count {
                return Err(format!(
                    "multiway checkpoint has invalid player {}",
                    entry.player
                ));
            }
            if !keys.insert((entry.player, entry.infoset)) {
                return Err(format!(
                    "multiway checkpoint contains duplicate information set {}:{}",
                    entry.player, entry.infoset
                ));
            }
            if entry.regret_sum.is_empty() || entry.regret_sum.len() != entry.strategy_sum.len() {
                return Err(format!(
                    "multiway checkpoint information set {}:{} has invalid action vectors",
                    entry.player, entry.infoset
                ));
            }
            if !entry
                .regret_sum
                .iter()
                .chain(entry.strategy_sum.iter())
                .all(|value| value.is_finite() && *value >= 0.0)
            {
                return Err(format!(
                    "multiway checkpoint information set {}:{} has invalid values",
                    entry.player, entry.infoset
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MultiwayMccfrSolver {
    game: MultiwayStaticGame,
    infosets: HashMap<(usize, InfoSetId), InfoSetData>,
    iterations: u64,
    rng: XorShift64,
    config_fingerprint: u64,
    metrics: MultiwaySamplingMetrics,
}

impl MultiwayMccfrSolver {
    pub fn new(
        game: MultiwayStaticGame,
        seed: u64,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        game.validate()?;
        Ok(Self {
            game,
            infosets: HashMap::new(),
            iterations: 0,
            rng: XorShift64::new(seed),
            config_fingerprint,
            metrics: MultiwaySamplingMetrics::default(),
        })
    }

    pub fn game(&self) -> &MultiwayStaticGame {
        &self.game
    }

    pub fn config_fingerprint(&self) -> u64 {
        self.config_fingerprint
    }

    pub fn iterations(&self) -> u64 {
        self.iterations
    }

    pub fn metrics(&self) -> MultiwaySamplingMetrics {
        self.metrics
    }

    pub fn average_strategy(&self, player: usize, infoset: InfoSetId) -> Option<Vec<f64>> {
        self.infosets
            .get(&(player, infoset))
            .map(InfoSetData::average_strategy)
    }

    pub fn current_strategy(&self, player: usize, infoset: InfoSetId) -> Option<Vec<f64>> {
        self.infosets
            .get(&(player, infoset))
            .map(InfoSetData::current_strategy)
    }

    pub fn info(&self, player: usize, infoset: InfoSetId) -> Option<&InfoSetData> {
        self.infosets.get(&(player, infoset))
    }

    pub fn checkpoint(&self) -> MultiwaySolverCheckpoint {
        let mut infosets: Vec<_> = self
            .infosets
            .iter()
            .map(|(&(player, infoset), data)| MultiwayInfoSetCheckpoint {
                player,
                infoset,
                regret_sum: data.regret_sum.clone(),
                strategy_sum: data.strategy_sum.clone(),
                visits: data.visits,
            })
            .collect();
        infosets.sort_by_key(|entry| (entry.player, entry.infoset));
        MultiwaySolverCheckpoint {
            format_version: MULTIWAY_CHECKPOINT_FORMAT_VERSION,
            algorithm: MultiwayAlgorithm::ExternalSamplingMccfr,
            player_count: self.game.player_count(),
            game_fingerprint: self.game.fingerprint(),
            config_fingerprint: self.config_fingerprint,
            iterations: self.iterations,
            infosets,
            rng_state: Some(self.rng.state),
            metrics: self.metrics,
        }
    }

    pub fn checkpoint_json(&self) -> Result<String, String> {
        self.checkpoint().to_json()
    }

    pub fn from_checkpoint(
        game: MultiwayStaticGame,
        checkpoint: &MultiwaySolverCheckpoint,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        checkpoint.validate()?;
        game.validate()?;
        if checkpoint.player_count != game.player_count() {
            return Err(format!(
                "multiway checkpoint player count {} does not match game {}",
                checkpoint.player_count,
                game.player_count()
            ));
        }
        if checkpoint.game_fingerprint != game.fingerprint() {
            return Err("multiway checkpoint game fingerprint does not match game".to_string());
        }
        if checkpoint.config_fingerprint != config_fingerprint {
            return Err("multiway checkpoint config fingerprint does not match".to_string());
        }
        let rng_state = checkpoint
            .rng_state
            .ok_or_else(|| "multiway checkpoint is missing RNG state".to_string())?;

        let mut action_counts = HashMap::new();
        for node in game.nodes() {
            if let MultiwayGameNode::Decision {
                player,
                infoset,
                children,
            } = node
            {
                if let Some(previous) = action_counts.insert((*player, *infoset), children.len()) {
                    if previous != children.len() {
                        return Err(format!(
                            "information set {}:{} changes action count",
                            player, infoset
                        ));
                    }
                }
            }
        }
        let mut infosets = HashMap::new();
        for entry in &checkpoint.infosets {
            let expected = action_counts
                .get(&(entry.player, entry.infoset))
                .copied()
                .ok_or_else(|| {
                    format!(
                        "multiway checkpoint references absent information set {}:{}",
                        entry.player, entry.infoset
                    )
                })?;
            if entry.regret_sum.len() != expected || entry.strategy_sum.len() != expected {
                return Err(format!(
                    "multiway checkpoint action count mismatch for information set {}:{}",
                    entry.player, entry.infoset
                ));
            }
            infosets.insert(
                (entry.player, entry.infoset),
                InfoSetData {
                    regret_sum: entry.regret_sum.clone(),
                    strategy_sum: entry.strategy_sum.clone(),
                    visits: entry.visits,
                },
            );
        }

        Ok(Self {
            game,
            infosets,
            iterations: checkpoint.iterations,
            rng: XorShift64 { state: rng_state },
            config_fingerprint,
            metrics: checkpoint.metrics,
        })
    }

    pub fn run(&mut self, iterations: u64) -> Result<(), String> {
        for _ in 0..iterations {
            for traverser in 0..self.game.player_count() {
                let reach = vec![1.0; self.game.player_count()];
                self.traverse(self.game.root(), traverser, reach)?;
                self.metrics.traverser_updates += 1;
            }
            self.iterations += 1;
        }
        Ok(())
    }

    pub fn evaluate_average_strategy(&self) -> Result<Vec<f64>, String> {
        self.evaluate_average_node(self.game.root())
    }

    fn evaluate_average_node(&self, node_id: MultiwayNodeId) -> Result<Vec<f64>, String> {
        let node = self
            .game
            .node(node_id)
            .ok_or_else(|| format!("unknown multiway node {node_id}"))?;
        match node {
            MultiwayGameNode::Terminal { utility } => Ok(utility.clone()),
            MultiwayGameNode::Chance { outcomes } => {
                let mut utility = vec![0.0; self.game.player_count()];
                for &(probability, child) in outcomes {
                    let child_utility = self.evaluate_average_node(child)?;
                    for (total, value) in utility.iter_mut().zip(child_utility) {
                        *total += probability * value;
                    }
                }
                Ok(utility)
            }
            MultiwayGameNode::Decision {
                player,
                infoset,
                children,
            } => {
                let strategy = self.strategy_for(*player, *infoset, children.len());
                let mut utility = vec![0.0; self.game.player_count()];
                for (&probability, &child) in strategy.iter().zip(children) {
                    let child_utility = self.evaluate_average_node(child)?;
                    for (total, value) in utility.iter_mut().zip(child_utility) {
                        *total += probability * value;
                    }
                }
                Ok(utility)
            }
        }
    }

    fn strategy_for(&self, player: usize, infoset: InfoSetId, action_count: usize) -> Vec<f64> {
        self.infosets
            .get(&(player, infoset))
            .map(InfoSetData::average_strategy)
            .unwrap_or_else(|| vec![1.0 / action_count as f64; action_count])
    }

    fn traverse(
        &mut self,
        node_id: MultiwayNodeId,
        traverser: usize,
        reach: Vec<f64>,
    ) -> Result<f64, String> {
        let node = self
            .game
            .node(node_id)
            .ok_or_else(|| format!("unknown multiway node {node_id}"))?
            .clone();
        match node {
            MultiwayGameNode::Terminal { utility } => Ok(utility[traverser]),
            MultiwayGameNode::Chance { outcomes } => {
                let probabilities: Vec<f64> = outcomes
                    .iter()
                    .map(|(probability, _)| *probability)
                    .collect();
                let selected = self.rng.sample_index(&probabilities);
                self.metrics.sampled_chance_nodes += 1;
                self.traverse(outcomes[selected].1, traverser, reach)
            }
            MultiwayGameNode::Decision {
                player,
                infoset,
                children,
            } => {
                let key = (player, infoset);
                let action_count = children.len();
                let strategy = {
                    let data = self
                        .infosets
                        .entry(key)
                        .or_insert_with(|| InfoSetData::new(action_count));
                    if data.regret_sum.len() != action_count {
                        return Err(format!(
                            "information set {player}:{infoset} changes action count from {} to {action_count}",
                            data.regret_sum.len()
                        ));
                    }
                    data.current_strategy()
                };

                if player != traverser {
                    let action = self.rng.sample_index(&strategy);
                    self.metrics.sampled_opponent_actions += 1;
                    let mut child_reach = reach;
                    child_reach[player] *= strategy[action];
                    return self.traverse(children[action], traverser, child_reach);
                }

                let mut action_values = Vec::with_capacity(action_count);
                for (action, &child) in children.iter().enumerate() {
                    let mut child_reach = reach.clone();
                    child_reach[player] *= strategy[action];
                    action_values.push(self.traverse(child, traverser, child_reach)?);
                }
                let node_value: f64 = strategy
                    .iter()
                    .zip(action_values.iter())
                    .map(|(probability, value)| probability * value)
                    .sum();
                let counterfactual_reach: f64 = reach
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != player)
                    .map(|(_, value)| *value)
                    .product();
                let strategy_reach = reach[player];
                let data = self
                    .infosets
                    .get_mut(&key)
                    .ok_or_else(|| "multiway information set disappeared".to_string())?;
                data.visits += 1;
                self.metrics.infoset_visits += 1;
                for action in 0..action_count {
                    let regret_delta = counterfactual_reach * (action_values[action] - node_value);
                    if !regret_delta.is_finite() {
                        return Err(format!(
                            "non-finite regret update at information set {player}:{infoset}"
                        ));
                    }
                    data.regret_sum[action] = (data.regret_sum[action] + regret_delta).max(0.0);
                    data.strategy_sum[action] += strategy_reach * strategy[action];
                }
                Ok(node_value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_player_reference_game() -> MultiwayStaticGame {
        let terminal = |utility: [f64; 3]| MultiwayGameNode::Terminal {
            utility: utility.to_vec(),
        };
        let nodes = vec![
            MultiwayGameNode::Chance {
                outcomes: vec![(0.5, 1), (0.5, 2)],
            },
            MultiwayGameNode::Decision {
                player: 0,
                infoset: 10,
                children: vec![3, 4],
            },
            MultiwayGameNode::Decision {
                player: 0,
                infoset: 10,
                children: vec![5, 6],
            },
            MultiwayGameNode::Decision {
                player: 1,
                infoset: 20,
                children: vec![7, 8],
            },
            MultiwayGameNode::Decision {
                player: 1,
                infoset: 20,
                children: vec![9, 10],
            },
            MultiwayGameNode::Decision {
                player: 1,
                infoset: 20,
                children: vec![11, 12],
            },
            MultiwayGameNode::Decision {
                player: 1,
                infoset: 20,
                children: vec![13, 14],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![15, 16],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![17, 18],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![19, 20],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![21, 22],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![23, 24],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![25, 26],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![27, 28],
            },
            MultiwayGameNode::Decision {
                player: 2,
                infoset: 30,
                children: vec![29, 30],
            },
            terminal([3.0, -1.0, -2.0]),
            terminal([-3.0, 1.0, 2.0]),
            terminal([2.0, 1.0, -3.0]),
            terminal([-2.0, -1.0, 3.0]),
            terminal([1.0, -3.0, 2.0]),
            terminal([-1.0, 3.0, -2.0]),
            terminal([2.0, -3.0, 1.0]),
            terminal([-2.0, 3.0, -1.0]),
            terminal([1.0, 2.0, -3.0]),
            terminal([-1.0, -2.0, 3.0]),
            terminal([3.0, 2.0, -5.0]),
            terminal([-3.0, -2.0, 5.0]),
            terminal([2.0, -1.0, -1.0]),
            terminal([-2.0, 1.0, 1.0]),
            terminal([1.0, -2.0, 1.0]),
            terminal([-1.0, 2.0, -1.0]),
        ];
        MultiwayStaticGame::new(3, 0, nodes).unwrap()
    }

    #[test]
    fn validates_dynamic_utility_vectors_and_rejects_cycles() {
        let invalid = MultiwayStaticGame::new(
            3,
            0,
            vec![MultiwayGameNode::Terminal {
                utility: vec![0.0, 0.0],
            }],
        );
        assert!(invalid.is_err());

        let cyclic = MultiwayStaticGame::new(
            3,
            0,
            vec![MultiwayGameNode::Decision {
                player: 0,
                infoset: 1,
                children: vec![0],
            }],
        );
        assert!(cyclic.is_err());
    }

    #[test]
    fn external_sampling_checkpoint_resume_is_deterministic_for_three_players() {
        let game = three_player_reference_game();
        let mut one_shot = MultiwayMccfrSolver::new(game.clone(), 1234, 77).unwrap();
        one_shot.run(8).unwrap();

        let mut split = MultiwayMccfrSolver::new(game.clone(), 1234, 77).unwrap();
        split.run(4).unwrap();
        let checkpoint = split.checkpoint();
        assert!(checkpoint.metrics.sampled_chance_nodes > 0);
        assert!(checkpoint.metrics.sampled_opponent_actions > 0);
        let mut resumed = MultiwayMccfrSolver::from_checkpoint(game, &checkpoint, 77).unwrap();
        resumed.run(4).unwrap();

        assert_eq!(resumed.iterations(), one_shot.iterations());
        assert_eq!(
            resumed.checkpoint().rng_state,
            one_shot.checkpoint().rng_state
        );
        assert_eq!(
            resumed.checkpoint().infosets.len(),
            one_shot.checkpoint().infosets.len()
        );
        for (resumed_entry, one_shot_entry) in resumed
            .checkpoint()
            .infosets
            .iter()
            .zip(one_shot.checkpoint().infosets.iter())
        {
            assert_eq!(
                (resumed_entry.player, resumed_entry.infoset),
                (one_shot_entry.player, one_shot_entry.infoset)
            );
            assert_eq!(resumed_entry.visits, one_shot_entry.visits);
            for (left, right) in resumed_entry
                .regret_sum
                .iter()
                .zip(one_shot_entry.regret_sum.iter())
            {
                assert!((left - right).abs() < 1e-12);
            }
        }
        let utility = resumed.evaluate_average_strategy().unwrap();
        assert_eq!(utility.len(), 3);
        assert!(utility.iter().all(|value| value.is_finite()));
    }
}
