//! Solver-ready extensive-form game interface and a compact CFR+ engine.
//!
//! The engine is intentionally independent of Hold'em rules. Hold'em trees can
//! implement the same node interface later; the matching-pennies toy game in
//! this crate validates regret updates before poker-specific integration.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub mod drill;
pub mod holdem;
pub mod multiway;
pub mod multiway_batch;
pub mod multiway_holdem;
pub mod multiway_job;
pub mod multiway_spot;

pub use drill::{
    run_drill, DrillFrequency, DrillHandOutcome, DrillHandRecord, DrillParams, DrillPrompt,
    DrillResponder, DrillSession, DRILL_DEFAULT_HANDS, DRILL_SESSION_SCHEMA_VERSION,
};

pub mod multiway_spot_json;

pub use holdem::{
    aggregate_strategy_report_by_deals, aggregate_strategy_report_from_ranges,
    build_strategy_report, compile_holdem_tree, compile_private_holdem_tree,
    private_deals_from_ranges, resume_compiled_holdem_game,
    resume_compiled_holdem_game_with_config_fingerprint, resume_private_holdem_from_ranges,
    resume_private_holdem_from_ranges_with_config_fingerprint, run_compiled_holdem_game,
    solve_private_holdem_from_ranges, ActionExport, ActionReport, CompiledHoldemGame,
    HoldemChipEvPayoff, HoldemSolveResult, InfoSetReport, PrivateDeal, RangeActionReport,
    RangeClassReport, RangeMatrix, RangeMatrixCell, RangeMatrixReport, RangeStrategyReport,
    StrategyReport, TerminalPayoff, RANGE_MATRIX_SIZE, RANGE_RESULT_SCHEMA_VERSION,
};
pub use multiway::{
    MultiwayAlgorithm, MultiwayGameNode, MultiwayInfoSetCheckpoint, MultiwayMccfrSolver,
    MultiwayNodeId, MultiwaySamplingMetrics, MultiwaySolverCheckpoint, MultiwayStaticGame,
};
pub use multiway_batch::{
    multiway_holdem_tree_fingerprint, MultiwayBatchActionReport,
    MultiwayBatchBestResponsePlayerEstimate, MultiwayBatchBestResponseReport,
    MultiwayBatchConvergenceDiagnostics, MultiwayBatchExploitabilityPlayerEstimate,
    MultiwayBatchExploitabilityReport, MultiwayBatchInfoSetCheckpoint, MultiwayBatchInfoSetReport,
    MultiwayBatchPlayerConvergence, MultiwayBatchSamplingMetrics, MultiwayBatchStrategyReport,
    MultiwayBatchUtilityEstimate, MultiwayCompiledProfileCacheMetrics,
    MultiwayHoldemBatchCheckpoint, MultiwayHoldemBatchSolver, MultiwayHoldemPublicArena,
    MultiwayHoldemPublicProfile, MULTIWAY_BATCH_RESULT_SCHEMA_VERSION,
};
pub use multiway_holdem::{
    compile_multiway_holdem_tree, multiway_private_deals_from_ranges,
    resume_compiled_multiway_holdem_game, resume_multiway_holdem_from_ranges,
    run_compiled_multiway_holdem_game, sample_multiway_private_deal,
    solve_multiway_holdem_from_ranges, MultiwayCompiledHoldemGame, MultiwayHoldemChipEvPayoff,
    MultiwayHoldemSolveResult, MultiwayPrivateDeal, MultiwayPrivateDealSample,
    MultiwayPrivateDealSampler, MultiwayTerminalPayoff,
};
pub use multiway_job::{
    MultiwayBatchJobConfig, MultiwayBatchJobManifest, MultiwayBatchJobStatus,
    MultiwayBatchJobStore, MULTIWAY_BATCH_JOB_FORMAT_VERSION,
};
pub use multiway_spot::{
    MultiwayHoldemSpotAction, MultiwayHoldemSpotConfig, MultiwayHoldemSpotHeroHandReport,
    MultiwayHoldemSpotHeroReport, MultiwayHoldemSpotResult, MultiwayHoldemSpotTreeConfig,
    MULTIWAY_HOLDEM_SPOT_RESULT_SCHEMA_VERSION,
};
pub use multiway_spot_json::{
    MultiwayHoldemSpotActionJson, MultiwayHoldemSpotActionKindJson, MultiwayHoldemSpotAnteModeJson,
    MultiwayHoldemSpotChanceJson, MultiwayHoldemSpotChanceOutcomeJson,
    MultiwayHoldemSpotClassWeightJson, MultiwayHoldemSpotExecutionJson, MultiwayHoldemSpotHeroJson,
    MultiwayHoldemSpotJob, MultiwayHoldemSpotRangeJson, MultiwayHoldemSpotStreetJson,
    MultiwayHoldemSpotTableJson, MultiwayHoldemSpotTreeJson, MultiwayHoldemSpotTreeModeJson,
    MULTIWAY_HOLDEM_SPOT_SCHEMA_VERSION,
};

pub mod card_abstraction;

pub type NodeId = usize;
pub type InfoSetId = u64;

#[derive(Debug, Clone)]
pub enum GameNode {
    Decision {
        player: usize,
        infoset: InfoSetId,
        children: Vec<NodeId>,
    },
    Chance {
        outcomes: Vec<(f64, NodeId)>,
    },
    Terminal {
        utility: [f64; 2],
    },
}

#[derive(Debug, Clone)]
pub struct StaticGame {
    root: NodeId,
    nodes: Vec<GameNode>,
}

impl StaticGame {
    pub fn new(root: NodeId, nodes: Vec<GameNode>) -> Result<Self, String> {
        let game = Self { root, nodes };
        game.validate()?;
        Ok(game)
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn node(&self, id: NodeId) -> Option<&GameNode> {
        self.nodes.get(id)
    }

    pub fn nodes(&self) -> &[GameNode] {
        &self.nodes
    }

    /// Deterministic structural fingerprint used to reject checkpoints built
    /// for a different static game graph.
    pub fn fingerprint(&self) -> u64 {
        static_game_fingerprint(self)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("game has no nodes".to_string());
        }
        if self.root >= self.nodes.len() {
            return Err("game root is out of range".to_string());
        }
        for (id, node) in self.nodes.iter().enumerate() {
            match node {
                GameNode::Decision {
                    player, children, ..
                } => {
                    if *player > 1 {
                        return Err(format!("only two players are supported, got {player}"));
                    }
                    if children.is_empty() {
                        return Err(format!("decision node {id} has no actions"));
                    }
                    for &child in children {
                        if child >= self.nodes.len() {
                            return Err(format!("node {id} references invalid child {child}"));
                        }
                    }
                }
                GameNode::Chance { outcomes } => {
                    if outcomes.is_empty() {
                        return Err(format!("chance node {id} has no outcomes"));
                    }
                    let mut total = 0.0;
                    for &(probability, child) in outcomes {
                        if probability <= 0.0 || !probability.is_finite() {
                            return Err(format!("chance node {id} has invalid probability"));
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
                GameNode::Terminal { utility } => {
                    if !utility.iter().all(|value| value.is_finite()) {
                        return Err(format!("terminal node {id} has non-finite utility"));
                    }
                }
            }
        }
        Ok(())
    }
}

fn static_game_fingerprint(game: &StaticGame) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0001_0000_01b3;

    fn update(mut hash: u64, value: u64) -> u64 {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }

    let mut hash = update(OFFSET, game.root as u64);
    hash = update(hash, game.nodes.len() as u64);
    for node in &game.nodes {
        match node {
            GameNode::Decision {
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
            GameNode::Chance { outcomes } => {
                hash = update(hash, 2);
                hash = update(hash, outcomes.len() as u64);
                for &(probability, child) in outcomes {
                    hash = update(hash, probability.to_bits());
                    hash = update(hash, child as u64);
                }
            }
            GameNode::Terminal { utility } => {
                hash = update(hash, 3);
                for value in utility {
                    hash = update(hash, value.to_bits());
                }
            }
        }
    }
    hash
}

#[derive(Debug, Clone)]
pub struct InfoSetData {
    pub regret_sum: Vec<f64>,
    pub strategy_sum: Vec<f64>,
    pub visits: u64,
}

impl InfoSetData {
    fn new(action_count: usize) -> Self {
        Self {
            regret_sum: vec![0.0; action_count],
            strategy_sum: vec![0.0; action_count],
            visits: 0,
        }
    }

    fn current_strategy(&self) -> Vec<f64> {
        let positive_sum: f64 = self.regret_sum.iter().map(|regret| regret.max(0.0)).sum();
        if positive_sum <= 0.0 {
            return vec![1.0 / self.regret_sum.len() as f64; self.regret_sum.len()];
        }
        self.regret_sum
            .iter()
            .map(|regret| regret.max(0.0) / positive_sum)
            .collect()
    }

    fn average_strategy(&self) -> Vec<f64> {
        let total: f64 = self.strategy_sum.iter().sum();
        if total <= 0.0 {
            return vec![1.0 / self.strategy_sum.len() as f64; self.strategy_sum.len()];
        }
        self.strategy_sum
            .iter()
            .map(|value| value / total)
            .collect()
    }
}

const CHECKPOINT_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SolverAlgorithm {
    CfrPlus,
    ExternalSamplingMccfr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoSetCheckpoint {
    pub player: usize,
    pub infoset: InfoSetId,
    pub regret_sum: Vec<f64>,
    pub strategy_sum: Vec<f64>,
    pub visits: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SolverCheckpoint {
    pub format_version: u32,
    pub algorithm: SolverAlgorithm,
    pub game_fingerprint: u64,
    /// Hash of the external tree/action-abstraction/configuration context.
    /// Zero means that the caller used the backwards-compatible default.
    #[serde(default)]
    pub config_fingerprint: u64,
    pub iterations: u64,
    pub infosets: Vec<InfoSetCheckpoint>,
    pub rng_state: Option<u64>,
}

impl SolverCheckpoint {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != CHECKPOINT_FORMAT_VERSION {
            return Err(format!(
                "unsupported checkpoint format version: {}",
                self.format_version
            ));
        }
        let mut keys = HashSet::new();
        for entry in &self.infosets {
            if entry.player > 1 {
                return Err(format!("checkpoint has invalid player {}", entry.player));
            }
            if !keys.insert((entry.player, entry.infoset)) {
                return Err(format!(
                    "checkpoint contains duplicate information set {}:{}",
                    entry.player, entry.infoset
                ));
            }
            if entry.regret_sum.is_empty() || entry.regret_sum.len() != entry.strategy_sum.len() {
                return Err(format!(
                    "checkpoint information set {}:{} has invalid action vectors",
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
                    "checkpoint information set {}:{} has invalid values",
                    entry.player, entry.infoset
                ));
            }
        }
        if self.algorithm == SolverAlgorithm::CfrPlus && self.rng_state.is_some() {
            return Err("CFR+ checkpoint cannot contain RNG state".to_string());
        }
        if self.algorithm == SolverAlgorithm::ExternalSamplingMccfr && self.rng_state.is_none() {
            return Err("MCCFR checkpoint must contain RNG state".to_string());
        }
        if self.algorithm == SolverAlgorithm::ExternalSamplingMccfr && self.rng_state == Some(0) {
            return Err("MCCFR checkpoint cannot contain zero RNG state".to_string());
        }
        Ok(())
    }
}

fn checkpoint_from_infosets(
    algorithm: SolverAlgorithm,
    game: &StaticGame,
    config_fingerprint: u64,
    iterations: u64,
    infosets: &HashMap<(usize, InfoSetId), InfoSetData>,
    rng_state: Option<u64>,
) -> SolverCheckpoint {
    let mut entries: Vec<_> = infosets
        .iter()
        .map(|(&(player, infoset), data)| InfoSetCheckpoint {
            player,
            infoset,
            regret_sum: data.regret_sum.clone(),
            strategy_sum: data.strategy_sum.clone(),
            visits: data.visits,
        })
        .collect();
    entries.sort_by_key(|entry| (entry.player, entry.infoset));
    SolverCheckpoint {
        format_version: CHECKPOINT_FORMAT_VERSION,
        algorithm,
        game_fingerprint: game.fingerprint(),
        config_fingerprint,
        iterations,
        infosets: entries,
        rng_state,
    }
}

fn validate_checkpoint_for_game(
    checkpoint: &SolverCheckpoint,
    expected_algorithm: SolverAlgorithm,
    game: &StaticGame,
    expected_config_fingerprint: u64,
) -> Result<(), String> {
    game.validate()?;
    checkpoint.validate()?;
    if checkpoint.algorithm != expected_algorithm {
        return Err(format!(
            "checkpoint algorithm {:?} does not match {:?}",
            checkpoint.algorithm, expected_algorithm
        ));
    }
    if checkpoint.game_fingerprint != game.fingerprint() {
        return Err("checkpoint game fingerprint does not match the supplied game".to_string());
    }
    if checkpoint.config_fingerprint != expected_config_fingerprint {
        return Err(
            "checkpoint config fingerprint does not match the supplied context".to_string(),
        );
    }

    let mut action_counts = HashMap::new();
    for node in game.nodes() {
        if let GameNode::Decision {
            player,
            infoset,
            children,
        } = node
        {
            let key = (*player, *infoset);
            if let Some(previous) = action_counts.insert(key, children.len()) {
                if previous != children.len() {
                    return Err(format!(
                        "game information set {}:{} changes action count",
                        player, infoset
                    ));
                }
            }
        }
    }
    for entry in &checkpoint.infosets {
        let action_count = action_counts
            .get(&(entry.player, entry.infoset))
            .copied()
            .ok_or_else(|| {
                format!(
                    "checkpoint references unknown information set {}:{}",
                    entry.player, entry.infoset
                )
            })?;
        if action_count != entry.regret_sum.len() {
            return Err(format!(
                "checkpoint information set {}:{} has {} actions, game has {}",
                entry.player,
                entry.infoset,
                entry.regret_sum.len(),
                action_count
            ));
        }
    }
    Ok(())
}

fn infosets_from_checkpoint(
    checkpoint: &SolverCheckpoint,
) -> HashMap<(usize, InfoSetId), InfoSetData> {
    checkpoint
        .infosets
        .iter()
        .map(|entry| {
            (
                (entry.player, entry.infoset),
                InfoSetData {
                    regret_sum: entry.regret_sum.clone(),
                    strategy_sum: entry.strategy_sum.clone(),
                    visits: entry.visits,
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct CfrPlusSolver {
    game: StaticGame,
    infosets: HashMap<(usize, InfoSetId), InfoSetData>,
    iterations: u64,
    config_fingerprint: u64,
}

impl CfrPlusSolver {
    pub fn new(game: StaticGame) -> Result<Self, String> {
        Self::new_with_config_fingerprint(game, 0)
    }

    pub fn new_with_config_fingerprint(
        game: StaticGame,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        game.validate()?;
        Ok(Self {
            game,
            infosets: HashMap::new(),
            iterations: 0,
            config_fingerprint,
        })
    }

    pub fn config_fingerprint(&self) -> u64 {
        self.config_fingerprint
    }

    pub fn checkpoint(&self) -> SolverCheckpoint {
        checkpoint_from_infosets(
            SolverAlgorithm::CfrPlus,
            &self.game,
            self.config_fingerprint,
            self.iterations,
            &self.infosets,
            None,
        )
    }

    pub fn checkpoint_json(&self) -> Result<String, String> {
        self.checkpoint().to_json()
    }

    pub fn from_checkpoint(
        game: StaticGame,
        checkpoint: &SolverCheckpoint,
    ) -> Result<Self, String> {
        Self::from_checkpoint_with_config_fingerprint(game, checkpoint, 0)
    }

    pub fn from_checkpoint_with_config_fingerprint(
        game: StaticGame,
        checkpoint: &SolverCheckpoint,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        validate_checkpoint_for_game(
            checkpoint,
            SolverAlgorithm::CfrPlus,
            &game,
            config_fingerprint,
        )?;
        Ok(Self {
            game,
            infosets: infosets_from_checkpoint(checkpoint),
            iterations: checkpoint.iterations,
            config_fingerprint,
        })
    }

    pub fn from_checkpoint_json(game: StaticGame, json: &str) -> Result<Self, String> {
        let checkpoint = SolverCheckpoint::from_json(json)?;
        Self::from_checkpoint(game, &checkpoint)
    }

    pub fn from_checkpoint_json_with_config_fingerprint(
        game: StaticGame,
        json: &str,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        let checkpoint = SolverCheckpoint::from_json(json)?;
        Self::from_checkpoint_with_config_fingerprint(game, &checkpoint, config_fingerprint)
    }

    pub fn run(&mut self, iterations: u64) -> Result<(), String> {
        for _ in 0..iterations {
            let reach = [1.0, 1.0];
            self.traverse(self.game.root(), reach, 1.0)?;
            self.iterations += 1;
        }
        Ok(())
    }

    pub fn iterations(&self) -> u64 {
        self.iterations
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

    pub fn max_positive_regret(&self) -> f64 {
        self.infosets
            .values()
            .flat_map(|data| data.regret_sum.iter())
            .map(|regret| regret.max(0.0))
            .fold(0.0, f64::max)
    }

    /// Evaluates the current average strategy profile from the root.
    ///
    /// This is mainly a regression/diagnostic API: it makes it possible to
    /// check a toy game's value without exposing the mutable traversal state.
    pub fn evaluate_average_strategy(&self) -> Result<[f64; 2], String> {
        self.evaluate_average_node(self.game.root())
    }

    fn evaluate_average_node(&self, node_id: NodeId) -> Result<[f64; 2], String> {
        let node = self
            .game
            .node(node_id)
            .ok_or_else(|| format!("unknown game node {node_id}"))?;
        match node {
            GameNode::Terminal { utility } => Ok(*utility),
            GameNode::Chance { outcomes } => {
                let mut utility = [0.0, 0.0];
                for &(probability, child) in outcomes {
                    let child_utility = self.evaluate_average_node(child)?;
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
                let strategy = self
                    .infosets
                    .get(&(*player, *infoset))
                    .map(InfoSetData::average_strategy)
                    .unwrap_or_else(|| vec![1.0 / children.len() as f64; children.len()]);
                if strategy.len() != children.len() {
                    return Err(format!(
                        "information set {infoset} has strategy length {} for {} actions",
                        strategy.len(),
                        children.len()
                    ));
                }
                let mut utility = [0.0, 0.0];
                for (probability, &child) in strategy.iter().zip(children) {
                    let child_utility = self.evaluate_average_node(child)?;
                    utility[0] += probability * child_utility[0];
                    utility[1] += probability * child_utility[1];
                }
                Ok(utility)
            }
        }
    }

    fn traverse(
        &mut self,
        node_id: NodeId,
        reach: [f64; 2],
        chance_reach: f64,
    ) -> Result<[f64; 2], String> {
        let node = self
            .game
            .node(node_id)
            .ok_or_else(|| format!("unknown game node {node_id}"))?
            .clone();

        match node {
            GameNode::Terminal { utility } => Ok(utility),
            GameNode::Chance { outcomes } => {
                let mut utility = [0.0, 0.0];
                for (probability, child) in outcomes {
                    let child_utility = self.traverse(child, reach, chance_reach * probability)?;
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
                let key = (player, infoset);
                let action_count = children.len();
                let strategy = {
                    let data = self
                        .infosets
                        .entry(key)
                        .or_insert_with(|| InfoSetData::new(action_count));
                    if data.regret_sum.len() != action_count {
                        return Err(format!(
                            "information set {infoset} changes action count from {} to {action_count}",
                            data.regret_sum.len()
                        ));
                    }
                    data.current_strategy()
                };

                let mut action_utilities = Vec::with_capacity(action_count);
                for (action, child) in children.into_iter().enumerate() {
                    let mut child_reach = reach;
                    child_reach[player] *= strategy[action];
                    action_utilities.push(self.traverse(child, child_reach, chance_reach)?);
                }

                let mut node_utility = [0.0, 0.0];
                for action in 0..action_count {
                    node_utility[0] += strategy[action] * action_utilities[action][0];
                    node_utility[1] += strategy[action] * action_utilities[action][1];
                }

                let opponent = 1 - player;
                let counterfactual_reach = reach[opponent] * chance_reach;
                let strategy_reach = reach[player] * chance_reach;
                let data = self
                    .infosets
                    .get_mut(&key)
                    .ok_or_else(|| "information set disappeared during traversal".to_string())?;
                data.visits += 1;
                for action in 0..action_count {
                    let regret_delta = counterfactual_reach
                        * (action_utilities[action][player] - node_utility[player]);
                    // CFR+: cumulative regrets are projected onto the
                    // non-negative half-line after every update.
                    data.regret_sum[action] = (data.regret_sum[action] + regret_delta).max(0.0);
                    data.strategy_sum[action] += strategy_reach * strategy[action];
                }

                Ok(node_utility)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 7;
        value ^= value >> 9;
        value ^= value << 8;
        self.state = value;
        value
    }

    fn next_unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn sample_index(&mut self, probabilities: &[f64]) -> usize {
        let draw = self.next_unit();
        let mut cumulative = 0.0;
        for (index, &probability) in probabilities.iter().enumerate() {
            cumulative += probability;
            if draw < cumulative {
                return index;
            }
        }
        probabilities.len() - 1
    }
}

/// External-sampling Monte Carlo CFR for the same two-player static game
/// interface as `CfrPlusSolver`.
///
/// Each iteration traverses once for each player. Chance outcomes and the
/// opponent's actions are sampled; the traversing player's actions are
/// enumerated for regret updates. A deterministic seed makes regression tests
/// and local jobs reproducible.
#[derive(Debug, Clone)]
pub struct MccfrSolver {
    game: StaticGame,
    infosets: HashMap<(usize, InfoSetId), InfoSetData>,
    iterations: u64,
    rng: XorShift64,
    config_fingerprint: u64,
}

impl MccfrSolver {
    pub fn new(game: StaticGame, seed: u64) -> Result<Self, String> {
        Self::new_with_config_fingerprint(game, seed, 0)
    }

    pub fn new_with_config_fingerprint(
        game: StaticGame,
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
        })
    }

    pub fn config_fingerprint(&self) -> u64 {
        self.config_fingerprint
    }

    pub fn checkpoint(&self) -> SolverCheckpoint {
        checkpoint_from_infosets(
            SolverAlgorithm::ExternalSamplingMccfr,
            &self.game,
            self.config_fingerprint,
            self.iterations,
            &self.infosets,
            Some(self.rng.state),
        )
    }

    pub fn checkpoint_json(&self) -> Result<String, String> {
        self.checkpoint().to_json()
    }

    pub fn from_checkpoint(
        game: StaticGame,
        checkpoint: &SolverCheckpoint,
    ) -> Result<Self, String> {
        Self::from_checkpoint_with_config_fingerprint(game, checkpoint, 0)
    }

    pub fn from_checkpoint_with_config_fingerprint(
        game: StaticGame,
        checkpoint: &SolverCheckpoint,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        validate_checkpoint_for_game(
            checkpoint,
            SolverAlgorithm::ExternalSamplingMccfr,
            &game,
            config_fingerprint,
        )?;
        let rng_state = checkpoint
            .rng_state
            .ok_or_else(|| "MCCFR checkpoint is missing RNG state".to_string())?;
        if rng_state == 0 {
            return Err("MCCFR checkpoint contains zero RNG state".to_string());
        }
        Ok(Self {
            game,
            infosets: infosets_from_checkpoint(checkpoint),
            iterations: checkpoint.iterations,
            rng: XorShift64 { state: rng_state },
            config_fingerprint,
        })
    }

    pub fn from_checkpoint_json(game: StaticGame, json: &str) -> Result<Self, String> {
        let checkpoint = SolverCheckpoint::from_json(json)?;
        Self::from_checkpoint(game, &checkpoint)
    }

    pub fn from_checkpoint_json_with_config_fingerprint(
        game: StaticGame,
        json: &str,
        config_fingerprint: u64,
    ) -> Result<Self, String> {
        let checkpoint = SolverCheckpoint::from_json(json)?;
        Self::from_checkpoint_with_config_fingerprint(game, &checkpoint, config_fingerprint)
    }

    pub fn run(&mut self, iterations: u64) -> Result<(), String> {
        for _ in 0..iterations {
            for traverser in 0..2 {
                self.traverse(self.game.root(), traverser, [1.0, 1.0])?;
            }
            self.iterations += 1;
        }
        Ok(())
    }

    pub fn iterations(&self) -> u64 {
        self.iterations
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

    pub fn max_positive_regret(&self) -> f64 {
        self.infosets
            .values()
            .flat_map(|data| data.regret_sum.iter())
            .map(|regret| regret.max(0.0))
            .fold(0.0, f64::max)
    }

    pub fn evaluate_average_strategy(&self) -> Result<[f64; 2], String> {
        self.evaluate_average_node(self.game.root())
    }

    fn evaluate_average_node(&self, node_id: NodeId) -> Result<[f64; 2], String> {
        let node = self
            .game
            .node(node_id)
            .ok_or_else(|| format!("unknown game node {node_id}"))?;
        match node {
            GameNode::Terminal { utility } => Ok(*utility),
            GameNode::Chance { outcomes } => {
                let mut utility = [0.0, 0.0];
                for &(probability, child) in outcomes {
                    let child_utility = self.evaluate_average_node(child)?;
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
                let strategy = self
                    .infosets
                    .get(&(*player, *infoset))
                    .map(InfoSetData::average_strategy)
                    .unwrap_or_else(|| vec![1.0 / children.len() as f64; children.len()]);
                if strategy.len() != children.len() {
                    return Err(format!(
                        "information set {infoset} has strategy length {} for {} actions",
                        strategy.len(),
                        children.len()
                    ));
                }
                let mut utility = [0.0, 0.0];
                for (probability, &child) in strategy.iter().zip(children) {
                    let child_utility = self.evaluate_average_node(child)?;
                    utility[0] += probability * child_utility[0];
                    utility[1] += probability * child_utility[1];
                }
                Ok(utility)
            }
        }
    }

    fn traverse(
        &mut self,
        node_id: NodeId,
        traverser: usize,
        reach: [f64; 2],
    ) -> Result<f64, String> {
        let node = self
            .game
            .node(node_id)
            .ok_or_else(|| format!("unknown game node {node_id}"))?
            .clone();
        match node {
            GameNode::Terminal { utility } => Ok(utility[traverser]),
            GameNode::Chance { outcomes } => {
                let probabilities: Vec<f64> = outcomes
                    .iter()
                    .map(|(probability, _)| *probability)
                    .collect();
                let selected = self.rng.sample_index(&probabilities);
                self.traverse(outcomes[selected].1, traverser, reach)
            }
            GameNode::Decision {
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
                            "information set {infoset} changes action count from {} to {action_count}",
                            data.regret_sum.len()
                        ));
                    }
                    data.current_strategy()
                };

                if player != traverser {
                    let action = self.rng.sample_index(&strategy);
                    let mut child_reach = reach;
                    child_reach[player] *= strategy[action];
                    return self.traverse(children[action], traverser, child_reach);
                }

                let mut action_values = Vec::with_capacity(action_count);
                for (action, &child) in children.iter().enumerate() {
                    let mut child_reach = reach;
                    child_reach[player] *= strategy[action];
                    action_values.push(self.traverse(child, traverser, child_reach)?);
                }
                let node_value: f64 = strategy
                    .iter()
                    .zip(action_values.iter())
                    .map(|(probability, value)| probability * value)
                    .sum();
                let counterfactual_reach = reach[1 - player];
                let strategy_reach = reach[player];
                let data = self
                    .infosets
                    .get_mut(&key)
                    .ok_or_else(|| "information set disappeared during traversal".to_string())?;
                data.visits += 1;
                for action in 0..action_count {
                    let regret_delta = counterfactual_reach * (action_values[action] - node_value);
                    data.regret_sum[action] = (data.regret_sum[action] + regret_delta).max(0.0);
                    data.strategy_sum[action] += strategy_reach * strategy[action];
                }
                Ok(node_value)
            }
        }
    }
}

/// Sequential representation of simultaneous matching pennies. Player 1 has
/// the same information set after either action of player 0, so CFR must learn
/// the 50/50 mixed equilibrium.
pub fn matching_pennies() -> StaticGame {
    let nodes = vec![
        GameNode::Decision {
            player: 0,
            infoset: 0,
            children: vec![1, 2],
        },
        GameNode::Decision {
            player: 1,
            infoset: 1,
            children: vec![3, 4],
        },
        GameNode::Decision {
            player: 1,
            infoset: 1,
            children: vec![5, 6],
        },
        GameNode::Terminal {
            utility: [1.0, -1.0],
        },
        GameNode::Terminal {
            utility: [-1.0, 1.0],
        },
        GameNode::Terminal {
            utility: [-1.0, 1.0],
        },
        GameNode::Terminal {
            utility: [1.0, -1.0],
        },
    ];
    StaticGame::new(0, nodes).expect("matching pennies game is valid")
}

/// Builds the classic three-card Kuhn poker reference game.
///
/// Cards are ordered J < Q < K. The chance node deals an ordered pair and
/// player decisions share information-set ids across deals that look the same
/// to the acting player. This makes the game a useful imperfect-information
/// regression target for CFR+ before private-card Hold'em conditioning exists.
pub fn kuhn_poker() -> StaticGame {
    const JACK: usize = 0;
    const QUEEN: usize = 1;
    const KING: usize = 2;

    fn showdown(card_zero: usize, card_one: usize, pot: f64) -> GameNode {
        if card_zero > card_one {
            GameNode::Terminal {
                utility: [pot, -pot],
            }
        } else {
            GameNode::Terminal {
                utility: [-pot, pot],
            }
        }
    }

    let mut nodes = vec![GameNode::Chance {
        outcomes: Vec::new(),
    }];
    let mut outcomes = Vec::new();

    for card_zero in [JACK, QUEEN, KING] {
        for card_one in [JACK, QUEEN, KING] {
            if card_zero == card_one {
                continue;
            }

            // Player 0 checks. Player 1 can check to showdown or bet; after a
            // bet, player 0 chooses fold/call.
            let p0_after_bet_call = nodes.len();
            nodes.push(showdown(card_zero, card_one, 2.0));
            let p0_after_bet_fold = nodes.len();
            nodes.push(GameNode::Terminal {
                utility: [-1.0, 1.0],
            });
            let p0_after_check_bet = nodes.len();
            nodes.push(GameNode::Decision {
                player: 0,
                infoset: 200 + card_zero as u64,
                children: vec![p0_after_bet_fold, p0_after_bet_call],
            });

            let p1_check_showdown = nodes.len();
            nodes.push(showdown(card_zero, card_one, 1.0));
            let p1_after_check = nodes.len();
            nodes.push(GameNode::Decision {
                player: 1,
                infoset: 100 + card_one as u64,
                children: vec![p1_check_showdown, p0_after_check_bet],
            });

            // Player 0 bets. Player 1 can fold or call.
            let p1_bet_fold = nodes.len();
            nodes.push(GameNode::Terminal {
                utility: [1.0, -1.0],
            });
            let p1_bet_call = nodes.len();
            nodes.push(showdown(card_zero, card_one, 2.0));
            let p1_after_p0_bet = nodes.len();
            nodes.push(GameNode::Decision {
                player: 1,
                infoset: 300 + card_one as u64,
                children: vec![p1_bet_fold, p1_bet_call],
            });

            let p0_root = nodes.len();
            nodes.push(GameNode::Decision {
                player: 0,
                infoset: card_zero as u64,
                children: vec![p1_after_check, p1_after_p0_bet],
            });
            outcomes.push((1.0 / 6.0, p0_root));
        }
    }

    nodes[0] = GameNode::Chance { outcomes };
    StaticGame::new(0, nodes).expect("Kuhn poker game is valid")
}

/// Builds a one-bet-per-round Leduc Poker reference game.
///
/// The deck has two copies of each of three ranks. Private cards are dealt at
/// the root, a single public card is dealt between the two betting rounds, and
/// each round allows at most one bet. The reduced betting abstraction keeps the
/// reference game compact while exercising private-card and public-card
/// information-set keys.
pub fn leduc_poker() -> StaticGame {
    fn showdown_utility(
        card_zero: usize,
        card_one: usize,
        board: usize,
        contributions: [f64; 2],
    ) -> [f64; 2] {
        let pair_zero = card_zero == board;
        let pair_one = card_one == board;
        let winner: Option<usize> = if pair_zero && !pair_one {
            Some(0)
        } else if pair_one && !pair_zero {
            Some(1)
        } else if !pair_zero && !pair_one && card_zero != card_one {
            Some(if card_zero > card_one { 0 } else { 1 })
        } else {
            None
        };
        let pot = contributions[0] + contributions[1];
        match winner {
            Some(0) => [pot - contributions[0], -contributions[1]],
            Some(1) => [-contributions[0], pot - contributions[1]],
            None => [pot / 2.0 - contributions[0], pot / 2.0 - contributions[1]],
            Some(_) => unreachable!("Leduc has two players"),
        }
    }

    fn fold_utility(winner: usize, contributions: [f64; 2]) -> [f64; 2] {
        let pot = contributions[0] + contributions[1];
        if winner == 0 {
            [pot - contributions[0], -contributions[1]]
        } else {
            [-contributions[0], pot - contributions[1]]
        }
    }

    fn push_showdown(
        nodes: &mut Vec<GameNode>,
        card_zero: usize,
        card_one: usize,
        board: usize,
        contributions: [f64; 2],
    ) -> NodeId {
        let id = nodes.len();
        nodes.push(GameNode::Terminal {
            utility: showdown_utility(card_zero, card_one, board, contributions),
        });
        id
    }

    fn push_fold(nodes: &mut Vec<GameNode>, winner: usize, contributions: [f64; 2]) -> NodeId {
        let id = nodes.len();
        nodes.push(GameNode::Terminal {
            utility: fold_utility(winner, contributions),
        });
        id
    }

    fn push_board_chance(
        nodes: &mut Vec<GameNode>,
        card_zero: usize,
        card_one: usize,
        contributions: [f64; 2],
    ) -> NodeId {
        let mut outcomes = Vec::new();
        for board_card in 0..6 {
            if board_card == card_zero || board_card == card_one {
                continue;
            }
            let board_rank = board_card / 2;
            let postflop_root =
                push_postflop(nodes, card_zero, card_one, board_rank, contributions);
            outcomes.push((0.25, postflop_root));
        }
        let id = nodes.len();
        nodes.push(GameNode::Chance { outcomes });
        id
    }

    fn push_postflop(
        nodes: &mut Vec<GameNode>,
        card_zero: usize,
        card_one: usize,
        board: usize,
        contributions: [f64; 2],
    ) -> NodeId {
        let card_zero_rank = card_zero / 2;
        let card_one_rank = card_one / 2;
        let board_offset = board as u64 * 10;

        let p0_after_p1_bet_call = push_showdown(
            nodes,
            card_zero_rank,
            card_one_rank,
            board,
            [contributions[0] + 1.0, contributions[1] + 1.0],
        );
        let p0_after_p1_bet_fold = push_fold(nodes, 1, [contributions[0], contributions[1] + 1.0]);
        let p0_after_p1_bet = nodes.len();
        nodes.push(GameNode::Decision {
            player: 0,
            infoset: 2_200 + board_offset + card_zero_rank as u64,
            children: vec![p0_after_p1_bet_fold, p0_after_p1_bet_call],
        });

        let p1_check_showdown =
            push_showdown(nodes, card_zero_rank, card_one_rank, board, contributions);
        let p1_after_check = nodes.len();
        nodes.push(GameNode::Decision {
            player: 1,
            infoset: 2_100 + board_offset + card_one_rank as u64,
            children: vec![p1_check_showdown, p0_after_p1_bet],
        });

        let p1_after_p0_bet_fold = push_fold(nodes, 0, [contributions[0] + 1.0, contributions[1]]);
        let p1_after_p0_bet_call = push_showdown(
            nodes,
            card_zero_rank,
            card_one_rank,
            board,
            [contributions[0] + 1.0, contributions[1] + 1.0],
        );
        let p1_after_p0_bet = nodes.len();
        nodes.push(GameNode::Decision {
            player: 1,
            infoset: 2_300 + board_offset + card_one_rank as u64,
            children: vec![p1_after_p0_bet_fold, p1_after_p0_bet_call],
        });

        let postflop_root = nodes.len();
        nodes.push(GameNode::Decision {
            player: 0,
            infoset: 2_000 + board_offset + card_zero_rank as u64,
            children: vec![p1_after_check, p1_after_p0_bet],
        });
        postflop_root
    }

    fn push_preflop(nodes: &mut Vec<GameNode>, card_zero: usize, card_one: usize) -> NodeId {
        let card_zero_rank = card_zero / 2;
        let card_one_rank = card_one / 2;

        let p0_after_p1_bet_call = push_board_chance(nodes, card_zero, card_one, [2.0, 2.0]);
        let p0_after_p1_bet_fold = push_fold(nodes, 1, [1.0, 2.0]);
        let p0_after_p1_bet = nodes.len();
        nodes.push(GameNode::Decision {
            player: 0,
            infoset: 1_300 + card_zero_rank as u64,
            children: vec![p0_after_p1_bet_fold, p0_after_p1_bet_call],
        });

        let p1_after_p0_check_check = push_board_chance(nodes, card_zero, card_one, [1.0, 1.0]);
        let p1_after_p0_check = nodes.len();
        nodes.push(GameNode::Decision {
            player: 1,
            infoset: 1_100 + card_one_rank as u64,
            children: vec![p1_after_p0_check_check, p0_after_p1_bet],
        });

        let p1_after_p0_bet_fold = push_fold(nodes, 0, [2.0, 1.0]);
        let p1_after_p0_bet_call = push_board_chance(nodes, card_zero, card_one, [2.0, 2.0]);
        let p1_after_p0_bet = nodes.len();
        nodes.push(GameNode::Decision {
            player: 1,
            infoset: 1_200 + card_one_rank as u64,
            children: vec![p1_after_p0_bet_fold, p1_after_p0_bet_call],
        });

        let root = nodes.len();
        nodes.push(GameNode::Decision {
            player: 0,
            infoset: 1_000 + card_zero_rank as u64,
            children: vec![p1_after_p0_check, p1_after_p0_bet],
        });
        root
    }

    let mut nodes = vec![GameNode::Chance {
        outcomes: Vec::new(),
    }];
    let mut deal_outcomes = Vec::new();
    for card_zero in 0..6 {
        for card_one in 0..6 {
            if card_zero == card_one {
                continue;
            }
            let root = push_preflop(&mut nodes, card_zero, card_one);
            deal_outcomes.push((1.0 / 30.0, root));
        }
    }
    nodes[0] = GameNode::Chance {
        outcomes: deal_outcomes,
    };
    StaticGame::new(0, nodes).expect("Leduc poker game is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_pennies_game_is_valid() {
        let game = matching_pennies();
        assert_eq!(game.nodes().len(), 7);
        assert!(game.validate().is_ok());
    }

    #[test]
    fn cfr_plus_converges_to_matching_pennies_mix() {
        let mut solver = CfrPlusSolver::new(matching_pennies()).unwrap();
        solver.run(20_000).unwrap();

        let player_zero = solver.average_strategy(0, 0).unwrap();
        let player_one = solver.average_strategy(1, 1).unwrap();
        assert!((player_zero[0] - 0.5).abs() < 0.03, "{player_zero:?}");
        assert!((player_one[0] - 0.5).abs() < 0.03, "{player_one:?}");
        assert_eq!(solver.info(0, 0).unwrap().visits, 20_000);
        assert!(solver.max_positive_regret() >= 0.0);
    }

    #[test]
    fn cfr_plus_checkpoint_round_trips_json_and_resumes() {
        let game = matching_pennies();
        let mut original = CfrPlusSolver::new_with_config_fingerprint(game.clone(), 1234).unwrap();
        original.run(100).unwrap();

        let json = original.checkpoint_json().unwrap();
        let checkpoint = SolverCheckpoint::from_json(&json).unwrap();
        assert_eq!(checkpoint.algorithm, SolverAlgorithm::CfrPlus);
        assert_eq!(checkpoint.game_fingerprint, game.fingerprint());
        assert_eq!(checkpoint.config_fingerprint, 1234);

        let mut restored =
            CfrPlusSolver::from_checkpoint_with_config_fingerprint(game.clone(), &checkpoint, 1234)
                .unwrap();
        assert_eq!(restored.iterations(), 100);
        original.run(100).unwrap();
        restored.run(100).unwrap();
        assert_eq!(original.checkpoint(), restored.checkpoint());

        assert!(CfrPlusSolver::from_checkpoint(kuhn_poker(), &checkpoint).is_err());
        assert!(
            CfrPlusSolver::from_checkpoint_with_config_fingerprint(game, &checkpoint, 4321,)
                .is_err()
        );
    }

    #[test]
    fn external_sampling_mccfr_checkpoint_restores_rng_state() {
        let game = kuhn_poker();
        let mut original = MccfrSolver::new(game.clone(), 123).unwrap();
        original.run(100).unwrap();
        let checkpoint = SolverCheckpoint::from_json(&original.checkpoint_json().unwrap()).unwrap();
        let mut restored = MccfrSolver::from_checkpoint(game, &checkpoint).unwrap();

        original.run(100).unwrap();
        restored.run(100).unwrap();
        let left = original.checkpoint();
        let right = restored.checkpoint();
        assert_eq!(left.iterations, right.iterations);
        assert_eq!(left.rng_state, right.rng_state);
        assert_eq!(left.infosets.len(), right.infosets.len());
        for (left_entry, right_entry) in left.infosets.iter().zip(&right.infosets) {
            assert_eq!(
                (left_entry.player, left_entry.infoset),
                (right_entry.player, right_entry.infoset)
            );
            assert_eq!(left_entry.visits, right_entry.visits);
            for (left_value, right_value) in left_entry
                .regret_sum
                .iter()
                .chain(left_entry.strategy_sum.iter())
                .zip(
                    right_entry
                        .regret_sum
                        .iter()
                        .chain(right_entry.strategy_sum.iter()),
                )
            {
                assert!((left_value - right_value).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn external_sampling_mccfr_converges_to_matching_pennies_mix() {
        let mut solver = MccfrSolver::new(matching_pennies(), 7).unwrap();
        solver.run(20_000).unwrap();

        let player_zero = solver.average_strategy(0, 0).unwrap();
        let player_one = solver.average_strategy(1, 1).unwrap();
        assert!((player_zero[0] - 0.5).abs() < 0.06, "{player_zero:?}");
        assert!((player_one[0] - 0.5).abs() < 0.06, "{player_one:?}");
        assert_eq!(solver.iterations(), 20_000);
        assert!(solver.max_positive_regret() >= 0.0);
    }

    #[test]
    fn external_sampling_mccfr_handles_kuhn_chance_nodes() {
        let mut solver = MccfrSolver::new(kuhn_poker(), 11).unwrap();
        solver.run(30_000).unwrap();
        let value = solver.evaluate_average_strategy().unwrap();
        assert!((value[0] + 1.0 / 18.0).abs() < 0.05, "{value:?}");
        assert!((value[0] + value[1]).abs() < 1e-9, "{value:?}");
    }

    #[test]
    fn kuhn_poker_is_valid_and_cfr_plus_reaches_reference_value() {
        let game = kuhn_poker();
        assert!(game.validate().is_ok());

        let mut solver = CfrPlusSolver::new(game).unwrap();
        solver.run(30_000).unwrap();
        let value = solver.evaluate_average_strategy().unwrap();

        // With the first player's utility convention used by this game, the
        // Kuhn equilibrium value is -1/18. CFR+ should be close after a full
        // traversal over all six deals.
        assert!((value[0] + 1.0 / 18.0).abs() < 0.015, "{value:?}");
        assert!((value[0] + value[1]).abs() < 1e-9, "{value:?}");

        for infoset in [0, 1, 2, 100, 101, 102, 200, 201, 202, 300, 301, 302] {
            let strategy = solver.average_strategy(
                if infoset >= 100 && infoset < 200 || infoset >= 300 {
                    1
                } else {
                    0
                },
                infoset,
            );
            let strategy = strategy.unwrap_or_else(|| panic!("missing infoset {infoset}"));
            assert_eq!(strategy.len(), 2);
            assert!((strategy.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn one_bet_leduc_is_valid_and_has_zero_sum_cfr_value() {
        let game = leduc_poker();
        assert!(game.validate().is_ok());
        match game.node(game.root()).unwrap() {
            GameNode::Chance { outcomes } => {
                assert_eq!(outcomes.len(), 30);
                assert!(
                    (outcomes
                        .iter()
                        .map(|(probability, _)| probability)
                        .sum::<f64>()
                        - 1.0)
                        .abs()
                        < 1e-9
                );
            }
            other => panic!("expected private-card chance root, got {other:?}"),
        }

        let mut solver = CfrPlusSolver::new(game).unwrap();
        solver.run(2_000).unwrap();
        let value = solver.evaluate_average_strategy().unwrap();
        assert!(value[0].is_finite() && value[1].is_finite());
        assert!((value[0] + value[1]).abs() < 1e-9, "{value:?}");

        for (player, infoset) in [(0, 1_000), (1, 1_100), (1, 1_200), (0, 2_000), (1, 2_100)] {
            let strategy = solver
                .average_strategy(player, infoset)
                .unwrap_or_else(|| panic!("missing infoset {player}:{infoset}"));
            assert_eq!(strategy.len(), 2);
            assert!((strategy.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        }
    }
}
