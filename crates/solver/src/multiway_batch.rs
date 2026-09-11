//! Batch-oriented sampled multiway Hold'em MCCFR.
//!
//! Unlike the finite-deal compiler, this layer samples one legal private-card
//! profile per traverser update and keeps regret/average-strategy data in a
//! shared store keyed by public node, actor, and the actor's own Combo. The
//! default path reuses an immutable public-tree arena and evaluates blocker
//! filtering and terminal utilities on demand; an optional bounded compiled
//! profile cache remains available for comparison and fallback.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use holdem_domain::{Action, PlayerId, PlayerStatus, Street};
use holdem_ranges::Combo;
use holdem_tree::{GameTree, LeafKind, NodeId as TreeNodeId, TreeNode};
use serde::{Deserialize, Serialize};

use crate::multiway_holdem::{
    compile_multiway_holdem_tree, MultiwayCompiledHoldemGame, MultiwayHoldemChipEvPayoff,
    MultiwayPrivateDeal, MultiwayPrivateDealSampler, MultiwayTerminalPayoff,
};
use crate::{ActionExport, InfoSetData, MultiwayGameNode, MultiwayNodeId, XorShift64};
use holdem_cards::{mask_from_cards, DeckMask};
use holdem_domain::table::Position;
use holdem_domain::TerminalState;
use holdem_ranges::WeightedRange;

const MULTIWAY_BATCH_CHECKPOINT_FORMAT_VERSION: u32 = 2;
const SAMPLER_SEED_MIX: u64 = 0xa076_1d64_78bd_642f;

fn default_profile_cache_capacity() -> usize {
    DEFAULT_PROFILE_CACHE_CAPACITY
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct BatchInfoSetKey {
    player: PlayerId,
    public_node: TreeNodeId,
    private_hand: Combo,
}

#[derive(Debug, Clone)]
struct BatchInfoSetData {
    action_count: usize,
    data: InfoSetData,
}

#[derive(Debug, Clone)]
struct BatchInfoSetDelta {
    key: BatchInfoSetKey,
    action_count: usize,
    regret_delta: Vec<f64>,
    strategy_delta: Vec<f64>,
    visits: u64,
}

#[derive(Debug, Clone, Copy)]
struct ParallelBatchTask {
    traverser: PlayerId,
    private_seed: u64,
    traversal_seed: u64,
}

#[derive(Debug, Clone)]
struct ParallelBatchTaskResult {
    traverser: PlayerId,
    private_sampling_attempts: u64,
    sampled_chance_nodes: u64,
    sampled_opponent_actions: u64,
    infoset_visits_by_player: Vec<u64>,
    updates: Vec<BatchInfoSetDelta>,
}

const DEFAULT_PROFILE_CACHE_CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiwayCompiledProfileCacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
}

#[derive(Debug, Clone)]
struct MultiwayCompiledProfileCache {
    capacity: usize,
    profiles: HashMap<Vec<Combo>, Arc<MultiwayCompiledHoldemGame>>,
    insertion_order: VecDeque<Vec<Combo>>,
    metrics: MultiwayCompiledProfileCacheMetrics,
}

impl MultiwayCompiledProfileCache {
    fn new(capacity: usize) -> Result<Self, String> {
        if capacity == 0 {
            return Err("compiled profile cache capacity must be positive".to_string());
        }
        Ok(Self {
            capacity,
            profiles: HashMap::new(),
            insertion_order: VecDeque::new(),
            metrics: MultiwayCompiledProfileCacheMetrics::default(),
        })
    }

    fn get_or_compile(
        &mut self,
        tree: &GameTree,
        deal: MultiwayPrivateDeal,
    ) -> Result<Arc<MultiwayCompiledHoldemGame>, String> {
        let key = deal.hands.clone();
        if let Some(compiled) = self.profiles.get(&key) {
            self.metrics.hits += 1;
            return Ok(Arc::clone(compiled));
        }

        self.metrics.misses += 1;
        let compiled = Arc::new(compile_multiway_holdem_tree(tree, &[deal])?);
        if self.profiles.len() >= self.capacity {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.profiles.remove(&oldest);
                self.metrics.evictions += 1;
            }
        }
        self.insertion_order.push_back(key.clone());
        self.profiles.insert(key, Arc::clone(&compiled));
        Ok(compiled)
    }

    fn metrics(&self) -> MultiwayCompiledProfileCacheMetrics {
        let mut metrics = self.metrics;
        metrics.entries = self.profiles.len();
        metrics
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchSamplingMetrics {
    pub traverser_updates: u64,
    pub sampled_private_deals: u64,
    pub private_sampling_attempts: u64,
    pub sampled_chance_nodes: u64,
    pub sampled_opponent_actions: u64,
    pub infoset_visits: u64,
    #[serde(default)]
    pub compiled_profile_cache_hits: u64,
    #[serde(default)]
    pub compiled_profile_cache_misses: u64,
    #[serde(default)]
    pub compiled_profile_cache_evictions: u64,
    /// Traverser updates split by traverser player. Empty vectors are accepted
    /// when loading a pre-diagnostics checkpoint and are initialized by the
    /// solver for the current table size.
    #[serde(default)]
    pub traverser_updates_by_player: Vec<u64>,
    /// Sampled private profiles and rejection-sampler attempts split by the
    /// traverser that requested the profile.
    #[serde(default)]
    pub sampled_private_deals_by_player: Vec<u64>,
    #[serde(default)]
    pub private_sampling_attempts_by_player: Vec<u64>,
    /// Decision information-set visits split by the acting player.
    #[serde(default)]
    pub infoset_visits_by_player: Vec<u64>,
}

impl MultiwayBatchSamplingMetrics {
    fn for_player_count(player_count: usize) -> Self {
        Self {
            traverser_updates_by_player: vec![0; player_count],
            sampled_private_deals_by_player: vec![0; player_count],
            private_sampling_attempts_by_player: vec![0; player_count],
            infoset_visits_by_player: vec![0; player_count],
            ..Self::default()
        }
    }

    fn validate_player_dimensions(&self, player_count: usize) -> Result<(), String> {
        for (name, values) in [
            (
                "traverser_updates_by_player",
                &self.traverser_updates_by_player,
            ),
            (
                "sampled_private_deals_by_player",
                &self.sampled_private_deals_by_player,
            ),
            (
                "private_sampling_attempts_by_player",
                &self.private_sampling_attempts_by_player,
            ),
            ("infoset_visits_by_player", &self.infoset_visits_by_player),
        ] {
            if !values.is_empty() && values.len() != player_count {
                return Err(format!(
                    "multiway batch metrics field {name} has {} players, expected {player_count}",
                    values.len()
                ));
            }
        }
        Ok(())
    }

    fn initialize_player_dimensions(&mut self, player_count: usize) -> Result<(), String> {
        self.validate_player_dimensions(player_count)?;
        if self.traverser_updates_by_player.is_empty() {
            self.traverser_updates_by_player = vec![0; player_count];
        }
        if self.sampled_private_deals_by_player.is_empty() {
            self.sampled_private_deals_by_player = vec![0; player_count];
        }
        if self.private_sampling_attempts_by_player.is_empty() {
            self.private_sampling_attempts_by_player = vec![0; player_count];
        }
        if self.infoset_visits_by_player.is_empty() {
            self.infoset_visits_by_player = vec![0; player_count];
        }
        Ok(())
    }
}

/// Monte Carlo estimate of average-profile utility over sampled private
/// profiles. Chance and opponent action branches are evaluated exactly for
/// each sampled profile; variance therefore measures private-profile sampling
/// variance, not the variance of the training traversal itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchUtilityEstimate {
    pub samples: u64,
    pub mean: Vec<f64>,
    pub variance: Vec<f64>,
    pub standard_error: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchPlayerConvergence {
    pub player: PlayerId,
    pub infosets: u64,
    pub visits: u64,
    pub positive_regret_sum: f64,
    /// Positive cumulative regret divided by this player's traverser updates.
    /// This is a diagnostic signal, not an exploitability proof.
    pub average_positive_regret: f64,
    /// Visit-weighted L1 distance between current regret matching and average
    /// strategy at the player's information sets.
    pub strategy_l1_distance: f64,
}

/// Lightweight convergence diagnostics derived from the shared sampled store.
/// The regret and strategy-distance values are useful for monitoring trends;
/// they do not replace an independent best-response probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchConvergenceDiagnostics {
    pub iterations: u64,
    pub player_count: usize,
    pub infosets: u64,
    pub visits: u64,
    pub positive_regret_sum: f64,
    pub average_positive_regret: f64,
    pub strategy_l1_distance: f64,
    pub players: Vec<MultiwayBatchPlayerConvergence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchBestResponsePlayerEstimate {
    pub player: PlayerId,
    pub samples: u64,
    pub strategy_value: f64,
    pub best_response_value: f64,
    pub improvement: f64,
    pub strategy_variance: f64,
    pub best_response_variance: f64,
    pub improvement_variance: f64,
    pub strategy_standard_error: f64,
    pub best_response_standard_error: f64,
    pub improvement_standard_error: f64,
}

/// Approximate per-player best-response values against the average sampled
/// strategy. The probe is paired on the same sampled private profiles, so the
/// improvement standard error is generally more informative than subtracting
/// two independent estimates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchBestResponseReport {
    pub player_count: usize,
    pub players: Vec<MultiwayBatchBestResponsePlayerEstimate>,
}

impl MultiwayBatchUtilityEstimate {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }
}

impl MultiwayBatchConvergenceDiagnostics {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }
}

impl MultiwayBatchBestResponseReport {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchInfoSetCheckpoint {
    pub player: PlayerId,
    pub public_node: TreeNodeId,
    pub private_cards: [u8; 2],
    pub action_count: usize,
    pub regret_sum: Vec<f64>,
    pub strategy_sum: Vec<f64>,
    pub visits: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayHoldemBatchCheckpoint {
    pub format_version: u32,
    pub player_count: usize,
    pub tree_fingerprint: u64,
    pub range_fingerprint: u64,
    pub dead_cards: DeckMask,
    pub config_fingerprint: u64,
    pub max_private_attempts: usize,
    #[serde(default = "default_profile_cache_capacity")]
    pub profile_cache_capacity: usize,
    pub iterations: u64,
    pub infosets: Vec<MultiwayBatchInfoSetCheckpoint>,
    pub traversal_rng_state: u64,
    pub private_rng_state: u64,
    pub private_sampling_attempts: u64,
    pub metrics: MultiwayBatchSamplingMetrics,
}

/// Version of the sampled multiway result schema.
pub const MULTIWAY_BATCH_RESULT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct MultiwayBatchActionReport {
    pub action: Action,
    pub frequency: f64,
    pub positive_regret: f64,
}

#[derive(Debug, Clone)]
pub struct MultiwayBatchInfoSetReport {
    pub player: PlayerId,
    pub public_node: TreeNodeId,
    pub private_hand: Combo,
    pub visits: u64,
    pub actions: Vec<MultiwayBatchActionReport>,
}

#[derive(Debug, Clone)]
pub struct MultiwayBatchStrategyReport {
    pub schema_version: u32,
    pub player_count: usize,
    pub iterations: u64,
    /// Utility estimated by evaluating the average profile on sampled private
    /// deals. It is not an exact full-range expectation.
    pub sampled_average_utility: Vec<f64>,
    pub utility_estimate: MultiwayBatchUtilityEstimate,
    pub convergence: MultiwayBatchConvergenceDiagnostics,
    pub metrics: MultiwayBatchSamplingMetrics,
    pub infosets: Vec<MultiwayBatchInfoSetReport>,
}

impl MultiwayHoldemBatchCheckpoint {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != MULTIWAY_BATCH_CHECKPOINT_FORMAT_VERSION {
            return Err(format!(
                "unsupported multiway batch checkpoint format version: {}",
                self.format_version
            ));
        }
        if !(3..=8).contains(&self.player_count) {
            return Err("multiway batch checkpoint player count must be 3-8".to_string());
        }
        if self.max_private_attempts == 0 {
            return Err("multiway batch checkpoint has zero private-attempt limit".to_string());
        }
        self.metrics.validate_player_dimensions(self.player_count)?;
        if self.traversal_rng_state == 0 || self.private_rng_state == 0 {
            return Err("multiway batch checkpoint contains zero RNG state".to_string());
        }
        let mut keys = std::collections::HashSet::new();
        for entry in &self.infosets {
            if entry.player >= self.player_count {
                return Err(format!(
                    "multiway batch checkpoint has invalid player {}",
                    entry.player
                ));
            }
            if entry.action_count == 0
                || entry.regret_sum.len() != entry.action_count
                || entry.strategy_sum.len() != entry.action_count
            {
                return Err(format!(
                    "multiway batch checkpoint information set {}:{} has invalid action vectors",
                    entry.player, entry.public_node
                ));
            }
            if !keys.insert((entry.player, entry.public_node, entry.private_cards)) {
                return Err(
                    "multiway batch checkpoint contains duplicate information set".to_string(),
                );
            }
            if !entry
                .regret_sum
                .iter()
                .chain(entry.strategy_sum.iter())
                .all(|value| value.is_finite() && *value >= 0.0)
            {
                return Err(
                    "multiway batch checkpoint contains non-finite strategy data".to_string(),
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct JsonMultiwayBatchStrategyReport {
    schema_version: u32,
    format: &'static str,
    player_count: usize,
    iterations: u64,
    sampled_average_utility: Vec<f64>,
    utility_estimate: MultiwayBatchUtilityEstimate,
    convergence: MultiwayBatchConvergenceDiagnostics,
    metrics: MultiwayBatchSamplingMetrics,
    infosets: Vec<JsonMultiwayBatchInfoSetReport>,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayBatchInfoSetReport {
    player: PlayerId,
    public_node: TreeNodeId,
    private_cards: [u8; 2],
    hand_class_id: u16,
    visits: u64,
    actions: Vec<JsonMultiwayBatchActionReport>,
}

#[derive(Debug, Serialize)]
struct JsonMultiwayBatchActionReport {
    action: ActionExport,
    frequency: f64,
    positive_regret: f64,
}

impl MultiwayBatchStrategyReport {
    pub fn to_json(&self) -> Result<String, String> {
        let export = JsonMultiwayBatchStrategyReport {
            schema_version: self.schema_version,
            format: "multiway_batch_strategy_report",
            player_count: self.player_count,
            iterations: self.iterations,
            sampled_average_utility: self.sampled_average_utility.clone(),
            utility_estimate: self.utility_estimate.clone(),
            convergence: self.convergence.clone(),
            metrics: self.metrics.clone(),
            infosets: self
                .infosets
                .iter()
                .map(|infoset| JsonMultiwayBatchInfoSetReport {
                    player: infoset.player,
                    public_node: infoset.public_node,
                    private_cards: infoset.private_hand.cards,
                    hand_class_id: infoset.private_hand.class_id(),
                    visits: infoset.visits,
                    actions: infoset
                        .actions
                        .iter()
                        .map(|action| JsonMultiwayBatchActionReport {
                            action: ActionExport::from_action(&action.action),
                            frequency: action.frequency,
                            positive_regret: action.positive_regret,
                        })
                        .collect(),
                })
                .collect(),
        };
        serde_json::to_string_pretty(&export).map_err(|error| error.to_string())
    }

    pub fn to_csv(&self) -> Result<String, String> {
        let mut lines = vec![
            "schema_version,format,player_count,iterations,player,public_node,private_card_0,private_card_1,hand_class_id,visits,action_kind,action_to,frequency,positive_regret".to_string(),
        ];
        for infoset in &self.infosets {
            for action in &infoset.actions {
                let (kind, target) = match &action.action {
                    Action::Fold => ("fold", String::new()),
                    Action::Check => ("check", String::new()),
                    Action::Call => ("call", String::new()),
                    Action::Bet { to } => ("bet", to.to_string()),
                    Action::Raise { to } => ("raise", to.to_string()),
                    Action::AllIn => ("all_in", String::new()),
                };
                let fields = [
                    self.schema_version.to_string(),
                    "multiway_batch_strategy".to_string(),
                    self.player_count.to_string(),
                    self.iterations.to_string(),
                    infoset.player.to_string(),
                    infoset.public_node.to_string(),
                    infoset.private_hand.cards[0].to_string(),
                    infoset.private_hand.cards[1].to_string(),
                    infoset.private_hand.class_id().to_string(),
                    infoset.visits.to_string(),
                    kind.to_string(),
                    target,
                    action.frequency.to_string(),
                    action.positive_regret.to_string(),
                ];
                lines.push(
                    fields
                        .iter()
                        .map(|field| csv_escape(field))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
        }
        Ok(format!("{}\n", lines.join("\n")))
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Deterministic structural fingerprint for a public Hold'em tree.
///
/// It includes state/accounting fields, action edges, chance outcomes and leaf
/// kinds, so a batch checkpoint cannot silently resume against a changed spot.
pub fn multiway_holdem_tree_fingerprint(tree: &GameTree) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0001_0000_01b3;
    fn update(mut hash: u64, value: u64) -> u64 {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }
    fn action_code(action: &Action) -> (u64, u64) {
        match action {
            Action::Fold => (1, 0),
            Action::Check => (2, 0),
            Action::Call => (3, 0),
            Action::Bet { to } => (4, *to as u64),
            Action::Raise { to } => (5, *to as u64),
            Action::AllIn => (6, 0),
        }
    }
    fn street_code(street: Street) -> u64 {
        match street {
            Street::Preflop => 0,
            Street::Flop => 1,
            Street::Turn => 2,
            Street::River => 3,
        }
    }
    fn status_code(status: PlayerStatus) -> u64 {
        match status {
            PlayerStatus::Active => 0,
            PlayerStatus::Folded => 1,
            PlayerStatus::AllIn => 2,
            PlayerStatus::OutOfHand => 3,
        }
    }
    fn position_code(position: Position) -> u64 {
        match position {
            Position::Unknown => 0,
            Position::ButtonSmallBlind => 1,
            Position::Button => 2,
            Position::SmallBlind => 3,
            Position::BigBlind => 4,
            Position::Utg => 5,
            Position::Utg1 => 6,
            Position::Lj => 7,
            Position::Hj => 8,
            Position::Co => 9,
        }
    }
    fn terminal_code(terminal: &TerminalState) -> (u64, u64) {
        match terminal {
            TerminalState::Fold { winner } => (1, *winner as u64),
            TerminalState::Showdown => (2, 0),
        }
    }

    let mut hash = update(OFFSET, tree.root as u64);
    hash = update(hash, tree.nodes.len() as u64);
    for node in &tree.nodes {
        hash = update(hash, node.id as u64);
        hash = update(hash, node.parent.unwrap_or(usize::MAX) as u64);
        hash = update(hash, node.state.table_size as u64);
        hash = update(hash, street_code(node.state.street));
        hash = update(hash, node.state.board.len() as u64);
        for &card in &node.state.board {
            hash = update(hash, card as u64);
        }
        hash = update(hash, node.state.players.len() as u64);
        for player in &node.state.players {
            hash = update(hash, player.seat as u64);
            hash = update(hash, position_code(player.position));
            hash = update(hash, player.stack_remaining as u64);
            hash = update(hash, player.committed_total as u64);
            hash = update(hash, player.committed_street as u64);
            hash = update(hash, player.ante_paid as u64);
            hash = update(hash, status_code(player.status));
        }
        hash = update(hash, node.state.dead_money as u64);
        hash = update(hash, node.state.pot as u64);
        hash = update(hash, node.state.actor.unwrap_or(usize::MAX) as u64);
        hash = update(hash, node.state.current_bet as u64);
        hash = update(hash, node.state.min_raise_increment as u64);
        hash = update(hash, node.state.pending_players.len() as u64);
        for &player in &node.state.pending_players {
            hash = update(hash, player as u64);
        }
        hash = update(hash, node.state.raise_allowed.len() as u64);
        for &player in &node.state.raise_allowed {
            hash = update(hash, player as u64);
        }
        hash = update(hash, node.children.len() as u64);
        for &child in &node.children {
            hash = update(hash, child as u64);
        }
        match &node.action_from_parent {
            Some(action) => {
                let (kind, value) = action_code(action);
                hash = update(hash, 1);
                hash = update(hash, kind);
                hash = update(hash, value);
            }
            None => hash = update(hash, 0),
        }
        match &node.leaf {
            Some(LeafKind::RoundComplete) => hash = update(hash, 1),
            Some(LeafKind::Terminal(terminal)) => {
                let (kind, value) = terminal_code(terminal);
                hash = update(hash, 2);
                hash = update(hash, kind);
                hash = update(hash, value);
            }
            None => hash = update(hash, 0),
        }
        match &node.chance {
            Some(chance) => {
                hash = update(hash, 1);
                hash = update(hash, street_code(chance.next_street));
                hash = update(hash, chance.outcomes.len() as u64);
                for outcome in &chance.outcomes {
                    hash = update(hash, outcome.cards.len() as u64);
                    for &card in &outcome.cards {
                        hash = update(hash, card as u64);
                    }
                    hash = update(hash, outcome.probability.to_bits());
                }
            }
            None => hash = update(hash, 0),
        }
    }
    hash
}

/// Immutable public-tree arena reused by every sampled private profile.
///
/// The arena owns only the validated public `GameTree`; private hands,
/// blocker-filtered chance outcomes, and terminal utilities are supplied by a
/// lightweight profile at traversal time.
#[derive(Debug, Clone)]
pub struct MultiwayHoldemPublicArena {
    tree: GameTree,
    player_count: usize,
    tree_fingerprint: u64,
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemPublicProfile {
    hands: Vec<Combo>,
    payoff: MultiwayHoldemChipEvPayoff,
}

impl MultiwayHoldemPublicArena {
    pub fn new(tree: GameTree) -> Result<Self, String> {
        tree.validate()?;
        let player_count = tree
            .node(tree.root)
            .ok_or_else(|| "public arena tree root is missing".to_string())?
            .state
            .table_size;
        if !(3..=8).contains(&player_count) {
            return Err(format!(
                "public multiway arena requires 3-8 seats, got {player_count}"
            ));
        }
        Ok(Self {
            tree_fingerprint: multiway_holdem_tree_fingerprint(&tree),
            tree,
            player_count,
        })
    }

    pub fn tree(&self) -> &GameTree {
        &self.tree
    }

    pub fn player_count(&self) -> usize {
        self.player_count
    }

    pub fn tree_fingerprint(&self) -> u64 {
        self.tree_fingerprint
    }

    pub fn profile(&self, hands: Vec<Combo>) -> Result<MultiwayHoldemPublicProfile, String> {
        if hands.len() != self.player_count {
            return Err(format!(
                "public profile has {} hands, expected {}",
                hands.len(),
                self.player_count
            ));
        }
        Ok(MultiwayHoldemPublicProfile {
            payoff: MultiwayHoldemChipEvPayoff::new(hands.clone())?,
            hands,
        })
    }
}

impl MultiwayHoldemPublicProfile {
    pub fn hands(&self) -> &[Combo] {
        &self.hands
    }

    fn hands_mask(&self) -> DeckMask {
        self.hands.iter().fold(0, |mask, hand| mask | hand.mask())
    }

    fn chance_outcomes(
        &self,
        tree: &GameTree,
        node_id: TreeNodeId,
    ) -> Result<Vec<(f64, TreeNodeId)>, String> {
        let node = tree
            .node(node_id)
            .ok_or_else(|| format!("unknown public tree node {node_id}"))?;
        let chance = node
            .chance
            .as_ref()
            .ok_or_else(|| format!("public node {node_id} is not a chance node"))?;
        let hands_mask = self.hands_mask();
        let mut allowed = Vec::with_capacity(chance.outcomes.len());
        let mut total = 0.0;
        for (outcome, &child) in chance.outcomes.iter().zip(&node.children) {
            let outcome_mask =
                mask_from_cards(&outcome.cards).map_err(|error| error.to_string())?;
            if outcome_mask & hands_mask != 0 {
                continue;
            }
            total += outcome.probability;
            allowed.push((outcome.probability, child));
        }
        if allowed.is_empty() || total <= 0.0 || !total.is_finite() {
            return Err(format!(
                "private profile has no legal public outcomes at node {node_id}"
            ));
        }
        Ok(allowed
            .into_iter()
            .map(|(probability, child)| (probability / total, child))
            .collect())
    }

    fn terminal_utility(&self, node: &TreeNode) -> Result<Vec<f64>, String> {
        self.payoff.utility(node)
    }
}

#[derive(Debug, Clone)]
pub struct MultiwayHoldemBatchSolver {
    public_arena: MultiwayHoldemPublicArena,
    ranges: Vec<WeightedRange>,
    player_count: usize,
    dead_cards: DeckMask,
    tree_fingerprint: u64,
    config_fingerprint: u64,
    max_private_attempts: usize,
    private_sampler: MultiwayPrivateDealSampler,
    traversal_rng: XorShift64,
    profile_cache: Option<MultiwayCompiledProfileCache>,
    infosets: HashMap<BatchInfoSetKey, BatchInfoSetData>,
    iterations: u64,
    metrics: MultiwayBatchSamplingMetrics,
}

impl MultiwayHoldemBatchSolver {
    pub fn new(
        tree: GameTree,
        ranges: Vec<WeightedRange>,
        dead_cards: DeckMask,
        seed: u64,
        config_fingerprint: u64,
        max_private_attempts: usize,
    ) -> Result<Self, String> {
        Self::new_with_profile_cache_capacity(
            tree,
            ranges,
            dead_cards,
            seed,
            config_fingerprint,
            max_private_attempts,
            0,
        )
    }

    pub fn new_with_profile_cache_capacity(
        tree: GameTree,
        ranges: Vec<WeightedRange>,
        dead_cards: DeckMask,
        seed: u64,
        config_fingerprint: u64,
        max_private_attempts: usize,
        profile_cache_capacity: usize,
    ) -> Result<Self, String> {
        tree.validate()?;
        let public_arena = MultiwayHoldemPublicArena::new(tree)?;
        if max_private_attempts == 0 {
            return Err("max_private_attempts must be positive".to_string());
        }
        let player_count = public_arena.player_count();
        if !(3..=8).contains(&player_count) || ranges.len() != player_count {
            return Err(format!(
                "batch solver requires 3-8 ranges matching table size {}, got {}",
                player_count,
                ranges.len()
            ));
        }
        let range_refs: Vec<&WeightedRange> = ranges.iter().collect();
        let private_sampler =
            MultiwayPrivateDealSampler::new(&range_refs, dead_cards, seed ^ SAMPLER_SEED_MIX)?;
        let profile_cache = if profile_cache_capacity == 0 {
            None
        } else {
            Some(MultiwayCompiledProfileCache::new(profile_cache_capacity)?)
        };
        Ok(Self {
            tree_fingerprint: public_arena.tree_fingerprint(),
            public_arena,
            ranges,
            player_count,
            dead_cards,
            config_fingerprint,
            max_private_attempts,
            private_sampler,
            traversal_rng: XorShift64::new(seed),
            profile_cache,
            infosets: HashMap::new(),
            iterations: 0,
            metrics: MultiwayBatchSamplingMetrics::for_player_count(player_count),
        })
    }

    pub fn iterations(&self) -> u64 {
        self.iterations
    }

    pub fn infoset_count(&self) -> usize {
        self.infosets.len()
    }

    /// Returns the average strategy for one exact private-card information set.
    pub fn average_strategy(
        &self,
        player: PlayerId,
        public_node: TreeNodeId,
        private_hand: Combo,
    ) -> Option<Vec<f64>> {
        self.infosets
            .get(&BatchInfoSetKey {
                player,
                public_node,
                private_hand,
            })
            .map(|entry| entry.data.average_strategy())
    }

    /// Returns the current regret-matching strategy for one information set.
    pub fn current_strategy(
        &self,
        player: PlayerId,
        public_node: TreeNodeId,
        private_hand: Combo,
    ) -> Option<Vec<f64>> {
        self.infosets
            .get(&BatchInfoSetKey {
                player,
                public_node,
                private_hand,
            })
            .map(|entry| entry.data.current_strategy())
    }

    pub fn metrics(&self) -> MultiwayBatchSamplingMetrics {
        self.metrics_with_cache()
    }

    pub fn compiled_profile_cache_metrics(&self) -> MultiwayCompiledProfileCacheMetrics {
        self.profile_cache
            .as_ref()
            .map(MultiwayCompiledProfileCache::metrics)
            .unwrap_or_default()
    }

    fn metrics_with_cache(&self) -> MultiwayBatchSamplingMetrics {
        let cache = self.compiled_profile_cache_metrics();
        let mut metrics = self.metrics.clone();
        metrics.compiled_profile_cache_hits = cache.hits;
        metrics.compiled_profile_cache_misses = cache.misses;
        metrics.compiled_profile_cache_evictions = cache.evictions;
        metrics
    }

    pub fn private_sampler(&self) -> &MultiwayPrivateDealSampler {
        &self.private_sampler
    }

    pub fn checkpoint(&self) -> MultiwayHoldemBatchCheckpoint {
        let mut infosets: Vec<_> = self
            .infosets
            .iter()
            .map(|(key, value)| MultiwayBatchInfoSetCheckpoint {
                player: key.player,
                public_node: key.public_node,
                private_cards: key.private_hand.cards,
                action_count: value.action_count,
                regret_sum: value.data.regret_sum.clone(),
                strategy_sum: value.data.strategy_sum.clone(),
                visits: value.data.visits,
            })
            .collect();
        infosets.sort_by_key(|entry| (entry.player, entry.public_node, entry.private_cards));
        MultiwayHoldemBatchCheckpoint {
            format_version: MULTIWAY_BATCH_CHECKPOINT_FORMAT_VERSION,
            player_count: self.player_count,
            tree_fingerprint: self.tree_fingerprint,
            range_fingerprint: self.private_sampler.range_fingerprint(),
            dead_cards: self.dead_cards,
            config_fingerprint: self.config_fingerprint,
            max_private_attempts: self.max_private_attempts,
            profile_cache_capacity: self
                .profile_cache
                .as_ref()
                .map(|cache| cache.capacity)
                .unwrap_or(0),
            iterations: self.iterations,
            infosets,
            traversal_rng_state: self.traversal_rng.state,
            private_rng_state: self.private_sampler.rng_state(),
            private_sampling_attempts: self.private_sampler.total_attempts(),
            metrics: self.metrics_with_cache(),
        }
    }

    pub fn checkpoint_json(&self) -> Result<String, String> {
        self.checkpoint().to_json()
    }

    pub fn from_checkpoint(
        tree: GameTree,
        ranges: Vec<WeightedRange>,
        dead_cards: DeckMask,
        checkpoint: &MultiwayHoldemBatchCheckpoint,
        config_fingerprint: u64,
        max_private_attempts: usize,
    ) -> Result<Self, String> {
        checkpoint.validate()?;
        let mut solver = Self::new_with_profile_cache_capacity(
            tree,
            ranges,
            dead_cards,
            1,
            config_fingerprint,
            max_private_attempts,
            checkpoint.profile_cache_capacity,
        )?;
        if checkpoint.player_count != solver.player_count
            || checkpoint.tree_fingerprint != solver.tree_fingerprint
            || checkpoint.config_fingerprint != solver.config_fingerprint
            || checkpoint.dead_cards != solver.dead_cards
            || checkpoint.max_private_attempts != solver.max_private_attempts
            || checkpoint.profile_cache_capacity
                != solver
                    .profile_cache
                    .as_ref()
                    .map(|cache| cache.capacity)
                    .unwrap_or(0)
            || checkpoint.range_fingerprint != solver.private_sampler.range_fingerprint()
        {
            return Err("multiway batch checkpoint context does not match solver".to_string());
        }
        let range_refs: Vec<&WeightedRange> = solver.ranges.iter().collect();
        solver.private_sampler = MultiwayPrivateDealSampler::from_state(
            &range_refs,
            dead_cards,
            checkpoint.private_rng_state,
            checkpoint.private_sampling_attempts,
        )?;
        solver.traversal_rng = XorShift64 {
            state: checkpoint.traversal_rng_state,
        };
        solver.iterations = checkpoint.iterations;
        solver.metrics = checkpoint.metrics.clone();
        solver
            .metrics
            .initialize_player_dimensions(solver.player_count)?;
        for entry in &checkpoint.infosets {
            let private_hand = Combo::new(entry.private_cards[0], entry.private_cards[1])?;
            solver.infosets.insert(
                BatchInfoSetKey {
                    player: entry.player,
                    public_node: entry.public_node,
                    private_hand,
                },
                BatchInfoSetData {
                    action_count: entry.action_count,
                    data: InfoSetData {
                        regret_sum: entry.regret_sum.clone(),
                        strategy_sum: entry.strategy_sum.clone(),
                        visits: entry.visits,
                    },
                },
            );
        }
        Ok(solver)
    }

    pub fn run(&mut self, iterations: u64) -> Result<(), String> {
        for _ in 0..iterations {
            for traverser in 0..self.player_count {
                let sample = self.private_sampler.sample(self.max_private_attempts)?;
                self.metrics.traverser_updates += 1;
                self.metrics.traverser_updates_by_player[traverser] += 1;
                self.metrics.sampled_private_deals += 1;
                self.metrics.sampled_private_deals_by_player[traverser] += 1;
                self.metrics.private_sampling_attempts += sample.attempts;
                self.metrics.private_sampling_attempts_by_player[traverser] += sample.attempts;
                let hands = sample.hands;
                if let Some(cache) = self.profile_cache.as_mut() {
                    let deal = MultiwayPrivateDeal {
                        probability: 1.0,
                        hands,
                    };
                    let compiled = cache.get_or_compile(self.public_arena.tree(), deal)?;
                    self.traverse(
                        compiled.as_ref(),
                        compiled.game().root(),
                        traverser,
                        vec![1.0; self.player_count],
                    )?;
                } else {
                    let profile = self.public_arena.profile(hands)?;
                    traverse_public_profile(
                        self.public_arena.tree(),
                        &profile,
                        self.public_arena.tree().root,
                        traverser,
                        vec![1.0; self.player_count],
                        &mut self.infosets,
                        &mut self.traversal_rng,
                        &mut self.metrics,
                    )?;
                }
            }
            self.iterations += 1;
        }
        Ok(())
    }

    fn evaluate_profile_sample(probe: &mut Self, hands: Vec<Combo>) -> Result<Vec<f64>, String> {
        if probe.profile_cache.is_some() {
            let deal = MultiwayPrivateDeal {
                probability: 1.0,
                hands,
            };
            let compiled = {
                let cache = probe
                    .profile_cache
                    .as_mut()
                    .ok_or_else(|| "compiled profile cache disappeared".to_string())?;
                cache.get_or_compile(probe.public_arena.tree(), deal)?
            };
            probe.evaluate_compiled_node(compiled.as_ref(), compiled.game().root())
        } else {
            let profile = probe.public_arena.profile(hands)?;
            evaluate_public_profile(probe.public_arena.tree(), &profile, &probe.infosets)
        }
    }

    /// Runs a deterministic batched MCCFR schedule using standard-library
    /// worker threads. Each reduction batch reads one immutable strategy
    /// snapshot; workers return raw regret/strategy deltas and the main thread
    /// applies them in task order. Consequently worker count does not affect
    /// the result, while `batch_size > 1` intentionally uses stale strategies
    /// inside one reduction batch.
    ///
    /// The parallel path is defined for the public-tree arena mode. The
    /// optional compiled-profile cache remains available through `run()` and
    /// is rejected here rather than silently changing cache semantics.
    pub fn run_parallel(
        &mut self,
        iterations: u64,
        worker_count: usize,
        batch_size: usize,
    ) -> Result<(), String> {
        if worker_count == 0 {
            return Err("parallel worker_count must be positive".to_string());
        }
        if batch_size == 0 {
            return Err("parallel batch_size must be positive".to_string());
        }
        if self.profile_cache.is_some() {
            return Err(
                "parallel multiway batching requires the public-tree arena mode; use new()"
                    .to_string(),
            );
        }

        let mut remaining = iterations;
        while remaining > 0 {
            let batch_iterations = remaining.min(batch_size as u64);
            let task_count_u64 = batch_iterations
                .checked_mul(self.player_count as u64)
                .ok_or_else(|| "parallel batch task count overflowed".to_string())?;
            let task_count = usize::try_from(task_count_u64)
                .map_err(|_| "parallel batch is too large for this platform".to_string())?;
            let snapshot = self.infosets.clone();
            let mut private_seed_rng = XorShift64 {
                state: self.private_sampler.rng_state(),
            };
            let mut traversal_seed_rng = self.traversal_rng.clone();
            let mut tasks = Vec::with_capacity(task_count);
            for _ in 0..batch_iterations {
                for traverser in 0..self.player_count {
                    tasks.push(ParallelBatchTask {
                        traverser,
                        private_seed: private_seed_rng.next_u64(),
                        traversal_seed: traversal_seed_rng.next_u64(),
                    });
                }
            }

            let worker_count = worker_count.min(task_count.max(1));
            let mut ordered_results: Vec<Option<ParallelBatchTaskResult>> =
                (0..task_count).map(|_| None).collect();
            std::thread::scope(|scope| -> Result<(), String> {
                let mut handles = Vec::with_capacity(worker_count);
                for worker_id in 0..worker_count {
                    let indices: Vec<usize> =
                        (worker_id..task_count).step_by(worker_count).collect();
                    let tree = self.public_arena.tree();
                    let sampler = &self.private_sampler;
                    let snapshot = &snapshot;
                    let tasks = &tasks;
                    let player_count = self.player_count;
                    let max_private_attempts = self.max_private_attempts;
                    handles.push(scope.spawn(move || {
                        let mut results = Vec::with_capacity(indices.len());
                        for index in indices {
                            let task = tasks[index];
                            let result = execute_parallel_batch_task(
                                tree,
                                sampler,
                                snapshot,
                                player_count,
                                max_private_attempts,
                                task,
                            );
                            results.push((index, result));
                        }
                        results
                    }));
                }

                for handle in handles {
                    let results = handle
                        .join()
                        .map_err(|_| "parallel multiway worker panicked".to_string())?;
                    for (index, result) in results {
                        ordered_results[index] = Some(result?);
                    }
                }
                Ok(())
            })?;

            let mut private_sampling_attempts = 0u64;
            for result in ordered_results {
                let result = result
                    .ok_or_else(|| "parallel worker did not return a task result".to_string())?;
                private_sampling_attempts = private_sampling_attempts
                    .checked_add(result.private_sampling_attempts)
                    .ok_or_else(|| "private sampling attempt counter overflowed".to_string())?;
                self.apply_parallel_batch_updates(&result.updates)?;
                self.metrics.traverser_updates += 1;
                self.metrics.traverser_updates_by_player[result.traverser] += 1;
                self.metrics.sampled_private_deals += 1;
                self.metrics.sampled_private_deals_by_player[result.traverser] += 1;
                self.metrics.private_sampling_attempts += result.private_sampling_attempts;
                self.metrics.private_sampling_attempts_by_player[result.traverser] +=
                    result.private_sampling_attempts;
                self.metrics.sampled_chance_nodes += result.sampled_chance_nodes;
                self.metrics.sampled_opponent_actions += result.sampled_opponent_actions;
                for (player, &visits) in result.infoset_visits_by_player.iter().enumerate() {
                    self.metrics.infoset_visits_by_player[player] += visits;
                    self.metrics.infoset_visits += visits;
                }
            }

            self.private_sampler = self.private_sampler.fork_with_state(
                private_seed_rng.state,
                self.private_sampler
                    .total_attempts()
                    .checked_add(private_sampling_attempts)
                    .ok_or_else(|| "private sampling attempt counter overflowed".to_string())?,
            )?;
            self.traversal_rng = traversal_seed_rng;
            self.iterations += batch_iterations;
            remaining -= batch_iterations;
        }
        Ok(())
    }

    fn apply_parallel_batch_updates(
        &mut self,
        updates: &[BatchInfoSetDelta],
    ) -> Result<(), String> {
        for update in updates {
            let entry = self
                .infosets
                .entry(update.key)
                .or_insert_with(|| BatchInfoSetData {
                    action_count: update.action_count,
                    data: InfoSetData::new(update.action_count),
                });
            if entry.action_count != update.action_count
                || entry.data.regret_sum.len() != update.action_count
                || update.regret_delta.len() != update.action_count
                || update.strategy_delta.len() != update.action_count
            {
                return Err(format!(
                    "parallel batch information set {}:{} changes action count",
                    update.key.player, update.key.public_node
                ));
            }
            for action in 0..update.action_count {
                let regret = entry.data.regret_sum[action] + update.regret_delta[action];
                let strategy = entry.data.strategy_sum[action] + update.strategy_delta[action];
                if !regret.is_finite() || !strategy.is_finite() {
                    return Err(
                        "parallel batch produced non-finite information-set data".to_string()
                    );
                }
                entry.data.regret_sum[action] = regret.max(0.0);
                entry.data.strategy_sum[action] = strategy;
            }
            entry.data.visits = entry
                .data
                .visits
                .checked_add(update.visits)
                .ok_or_else(|| "parallel information-set visit counter overflowed".to_string())?;
        }
        Ok(())
    }

    pub fn evaluate_average_utility_estimate(
        &self,
        sample_count: usize,
        max_private_attempts: usize,
    ) -> Result<MultiwayBatchUtilityEstimate, String> {
        if sample_count == 0 {
            return Err("sample_count must be positive".to_string());
        }
        let mut probe = self.clone();
        let mut mean = vec![0.0; self.player_count];
        let mut m2 = vec![0.0; self.player_count];
        for sample_index in 0..sample_count {
            let sample = probe.private_sampler.sample(max_private_attempts)?;
            let utility = Self::evaluate_profile_sample(&mut probe, sample.hands)?;
            if utility.len() != self.player_count || !utility.iter().all(|value| value.is_finite())
            {
                return Err("average-profile utility estimate contains invalid values".to_string());
            }
            let count = (sample_index + 1) as f64;
            for ((mean_value, m2_value), value) in mean.iter_mut().zip(m2.iter_mut()).zip(utility) {
                let delta = value - *mean_value;
                *mean_value += delta / count;
                let second_delta = value - *mean_value;
                *m2_value += delta * second_delta;
            }
        }
        let (variance, standard_error) = estimate_variance_and_error(&m2, sample_count);
        Ok(MultiwayBatchUtilityEstimate {
            samples: sample_count as u64,
            mean,
            variance,
            standard_error,
        })
    }

    pub fn evaluate_average_utility(
        &self,
        sample_count: usize,
        max_private_attempts: usize,
    ) -> Result<Vec<f64>, String> {
        Ok(self
            .evaluate_average_utility_estimate(sample_count, max_private_attempts)?
            .mean)
    }

    pub fn convergence_diagnostics(&self) -> MultiwayBatchConvergenceDiagnostics {
        let mut infosets_by_player = vec![0u64; self.player_count];
        let mut visits_by_player = vec![0u64; self.player_count];
        let mut regrets_by_player = vec![0.0; self.player_count];
        let mut weighted_strategy_distance_by_player = vec![0.0; self.player_count];

        for (key, entry) in &self.infosets {
            let player = key.player;
            if player >= self.player_count {
                continue;
            }
            infosets_by_player[player] += 1;
            visits_by_player[player] = visits_by_player[player].saturating_add(entry.data.visits);
            let positive_regret: f64 = entry
                .data
                .regret_sum
                .iter()
                .map(|value| value.max(0.0))
                .sum();
            regrets_by_player[player] += positive_regret;
            let current = entry.data.current_strategy();
            let average = entry.data.average_strategy();
            let l1_distance: f64 = current
                .iter()
                .zip(average.iter())
                .map(|(left, right)| (left - right).abs())
                .sum();
            weighted_strategy_distance_by_player[player] += entry.data.visits as f64 * l1_distance;
        }

        let mut players = Vec::with_capacity(self.player_count);
        for player in 0..self.player_count {
            let updates = self
                .metrics
                .traverser_updates_by_player
                .get(player)
                .copied()
                .unwrap_or(0);
            let average_positive_regret = if updates == 0 {
                0.0
            } else {
                regrets_by_player[player] / updates as f64
            };
            let strategy_l1_distance = if visits_by_player[player] == 0 {
                0.0
            } else {
                weighted_strategy_distance_by_player[player] / visits_by_player[player] as f64
            };
            players.push(MultiwayBatchPlayerConvergence {
                player,
                infosets: infosets_by_player[player],
                visits: visits_by_player[player],
                positive_regret_sum: regrets_by_player[player],
                average_positive_regret,
                strategy_l1_distance,
            });
        }

        let positive_regret_sum = regrets_by_player.iter().sum();
        let average_positive_regret = if self.metrics.traverser_updates == 0 {
            0.0
        } else {
            positive_regret_sum / self.metrics.traverser_updates as f64
        };
        let visits = visits_by_player.iter().sum();
        let strategy_l1_distance = if visits == 0 {
            0.0
        } else {
            weighted_strategy_distance_by_player.iter().sum::<f64>() / visits as f64
        };
        MultiwayBatchConvergenceDiagnostics {
            iterations: self.iterations,
            player_count: self.player_count,
            infosets: infosets_by_player.iter().sum(),
            visits,
            positive_regret_sum,
            average_positive_regret,
            strategy_l1_distance,
            players,
        }
    }

    pub fn best_response_probe(
        &self,
        sample_count: usize,
        max_private_attempts: usize,
    ) -> Result<MultiwayBatchBestResponseReport, String> {
        if sample_count == 0 {
            return Err("sample_count must be positive".to_string());
        }
        let mut probe = self.clone();
        let mut strategy_mean = vec![0.0; self.player_count];
        let mut strategy_m2 = vec![0.0; self.player_count];
        let mut best_response_mean = vec![0.0; self.player_count];
        let mut best_response_m2 = vec![0.0; self.player_count];
        let mut improvement_mean = vec![0.0; self.player_count];
        let mut improvement_m2 = vec![0.0; self.player_count];

        for sample_index in 0..sample_count {
            let sample = probe.private_sampler.sample(max_private_attempts)?;
            let profile = probe.public_arena.profile(sample.hands)?;
            let strategy_utility =
                evaluate_public_profile(probe.public_arena.tree(), &profile, &probe.infosets)?;
            if strategy_utility.len() != self.player_count
                || !strategy_utility.iter().all(|value| value.is_finite())
            {
                return Err("best-response probe received invalid strategy utility".to_string());
            }
            let count = (sample_index + 1) as f64;
            for player in 0..self.player_count {
                let best_response_value = evaluate_public_best_response(
                    probe.public_arena.tree(),
                    &profile,
                    &probe.infosets,
                    player,
                    probe.public_arena.tree().root,
                )?;
                if !best_response_value.is_finite() {
                    return Err("best-response probe produced a non-finite value".to_string());
                }
                update_online_moment(
                    &mut strategy_mean[player],
                    &mut strategy_m2[player],
                    strategy_utility[player],
                    count,
                );
                update_online_moment(
                    &mut best_response_mean[player],
                    &mut best_response_m2[player],
                    best_response_value,
                    count,
                );
                update_online_moment(
                    &mut improvement_mean[player],
                    &mut improvement_m2[player],
                    best_response_value - strategy_utility[player],
                    count,
                );
            }
        }

        let mut players = Vec::with_capacity(self.player_count);
        for player in 0..self.player_count {
            let (strategy_variance, strategy_standard_error) =
                scalar_variance_and_error(strategy_m2[player], sample_count);
            let (best_response_variance, best_response_standard_error) =
                scalar_variance_and_error(best_response_m2[player], sample_count);
            let (improvement_variance, improvement_standard_error) =
                scalar_variance_and_error(improvement_m2[player], sample_count);
            players.push(MultiwayBatchBestResponsePlayerEstimate {
                player,
                samples: sample_count as u64,
                strategy_value: strategy_mean[player],
                best_response_value: best_response_mean[player],
                improvement: improvement_mean[player],
                strategy_variance,
                best_response_variance,
                improvement_variance,
                strategy_standard_error,
                best_response_standard_error,
                improvement_standard_error,
            });
        }
        Ok(MultiwayBatchBestResponseReport {
            player_count: self.player_count,
            players,
        })
    }

    pub fn strategy_report(
        &self,
        utility_samples: usize,
        max_private_attempts: usize,
    ) -> Result<MultiwayBatchStrategyReport, String> {
        let utility_estimate =
            self.evaluate_average_utility_estimate(utility_samples, max_private_attempts)?;
        let sampled_average_utility = utility_estimate.mean.clone();
        let convergence = self.convergence_diagnostics();
        let mut infosets = Vec::with_capacity(self.infosets.len());
        for (key, entry) in &self.infosets {
            let public_node = self
                .public_arena
                .tree()
                .node(key.public_node)
                .ok_or_else(|| format!("unknown public node {}", key.public_node))?;
            let actions: Vec<Action> = public_node
                .children
                .iter()
                .map(|&child| {
                    self.public_arena
                        .tree()
                        .node(child)
                        .and_then(|node| node.action_from_parent.clone())
                        .ok_or_else(|| {
                            format!(
                                "public node {} has a child without an incoming action",
                                key.public_node
                            )
                        })
                })
                .collect::<Result<_, String>>()?;
            if actions.len() != entry.action_count || entry.data.regret_sum.len() != actions.len() {
                return Err(format!(
                    "strategy report action count mismatch at player {} node {}",
                    key.player, key.public_node
                ));
            }
            let frequencies = entry.data.average_strategy();
            infosets.push(MultiwayBatchInfoSetReport {
                player: key.player,
                public_node: key.public_node,
                private_hand: key.private_hand,
                visits: entry.data.visits,
                actions: actions
                    .into_iter()
                    .enumerate()
                    .map(|(index, action)| MultiwayBatchActionReport {
                        action,
                        frequency: frequencies[index],
                        positive_regret: entry.data.regret_sum[index].max(0.0),
                    })
                    .collect(),
            });
        }
        infosets.sort_by_key(|infoset| (infoset.player, infoset.public_node, infoset.private_hand));
        Ok(MultiwayBatchStrategyReport {
            schema_version: MULTIWAY_BATCH_RESULT_SCHEMA_VERSION,
            player_count: self.player_count,
            iterations: self.iterations,
            sampled_average_utility,
            utility_estimate,
            convergence,
            metrics: self.metrics(),
            infosets,
        })
    }

    fn batch_key(
        &mut self,
        compiled: &MultiwayCompiledHoldemGame,
        node_id: MultiwayNodeId,
        player: PlayerId,
        infoset: u64,
        action_count: usize,
    ) -> Result<BatchInfoSetKey, String> {
        let public_node = compiled
            .tree_node_id(node_id)
            .ok_or_else(|| format!("decision node {node_id} has no public node metadata"))?;
        let hands = compiled
            .private_hands_at(node_id)
            .ok_or_else(|| format!("decision node {node_id} has no private hands"))?;
        let private_hand = *hands
            .get(player)
            .ok_or_else(|| format!("player {player} is outside private hand vector"))?;
        let key = BatchInfoSetKey {
            player,
            public_node,
            private_hand,
        };
        let entry = self
            .infosets
            .entry(key)
            .or_insert_with(|| BatchInfoSetData {
                action_count,
                data: InfoSetData::new(action_count),
            });
        if entry.action_count != action_count || entry.data.regret_sum.len() != action_count {
            return Err(format!(
                "batch information set {player}:{infoset} changes action count"
            ));
        }
        Ok(key)
    }

    fn traverse(
        &mut self,
        compiled: &MultiwayCompiledHoldemGame,
        node_id: MultiwayNodeId,
        traverser: PlayerId,
        reach: Vec<f64>,
    ) -> Result<f64, String> {
        let node = compiled
            .game()
            .node(node_id)
            .ok_or_else(|| format!("unknown compiled multiway node {node_id}"))?
            .clone();
        match node {
            MultiwayGameNode::Terminal { utility } => Ok(utility[traverser]),
            MultiwayGameNode::Chance { outcomes } => {
                let probabilities: Vec<f64> = outcomes
                    .iter()
                    .map(|(probability, _)| *probability)
                    .collect();
                let selected = self.traversal_rng.sample_index(&probabilities);
                self.metrics.sampled_chance_nodes += 1;
                self.traverse(compiled, outcomes[selected].1, traverser, reach)
            }
            MultiwayGameNode::Decision {
                player,
                infoset,
                children,
            } => {
                let action_count = children.len();
                let key = self.batch_key(compiled, node_id, player, infoset, action_count)?;
                let strategy = self
                    .infosets
                    .get(&key)
                    .ok_or_else(|| "batch information set disappeared".to_string())?
                    .data
                    .current_strategy();

                if player != traverser {
                    let action = self.traversal_rng.sample_index(&strategy);
                    self.metrics.sampled_opponent_actions += 1;
                    let mut child_reach = reach;
                    child_reach[player] *= strategy[action];
                    return self.traverse(compiled, children[action], traverser, child_reach);
                }

                let mut action_values = Vec::with_capacity(action_count);
                for (action, &child) in children.iter().enumerate() {
                    let mut child_reach = reach.clone();
                    child_reach[player] *= strategy[action];
                    action_values.push(self.traverse(compiled, child, traverser, child_reach)?);
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
                    .ok_or_else(|| "batch information set disappeared during update".to_string())?;
                data.data.visits += 1;
                self.metrics.infoset_visits += 1;
                self.metrics.infoset_visits_by_player[player] += 1;
                for action in 0..action_count {
                    let regret_delta = counterfactual_reach * (action_values[action] - node_value);
                    if !regret_delta.is_finite() {
                        return Err("batch MCCFR produced non-finite regret".to_string());
                    }
                    data.data.regret_sum[action] =
                        (data.data.regret_sum[action] + regret_delta).max(0.0);
                    data.data.strategy_sum[action] += strategy_reach * strategy[action];
                }
                Ok(node_value)
            }
        }
    }

    fn evaluate_compiled_node(
        &self,
        compiled: &MultiwayCompiledHoldemGame,
        node_id: MultiwayNodeId,
    ) -> Result<Vec<f64>, String> {
        let node = compiled
            .game()
            .node(node_id)
            .ok_or_else(|| format!("unknown compiled multiway node {node_id}"))?;
        match node {
            MultiwayGameNode::Terminal { utility } => Ok(utility.clone()),
            MultiwayGameNode::Chance { outcomes } => {
                let mut utility = vec![0.0; self.player_count];
                for &(probability, child) in outcomes {
                    let child_utility = self.evaluate_compiled_node(compiled, child)?;
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
                let public_node = compiled
                    .tree_node_id(node_id)
                    .ok_or_else(|| "decision node has no public metadata".to_string())?;
                let hands = compiled
                    .private_hands_at(node_id)
                    .ok_or_else(|| "decision node has no private metadata".to_string())?;
                let private_hand = hands[*player];
                let key = BatchInfoSetKey {
                    player: *player,
                    public_node,
                    private_hand,
                };
                let strategy = self
                    .infosets
                    .get(&key)
                    .map(|entry| entry.data.average_strategy())
                    .unwrap_or_else(|| vec![1.0 / children.len() as f64; children.len()]);
                if strategy.len() != children.len() {
                    return Err(format!(
                        "batch information set {player}:{infoset} has wrong action count"
                    ));
                }
                let mut utility = vec![0.0; self.player_count];
                for (&probability, &child) in strategy.iter().zip(children) {
                    let child_utility = self.evaluate_compiled_node(compiled, child)?;
                    for (total, value) in utility.iter_mut().zip(child_utility) {
                        *total += probability * value;
                    }
                }
                Ok(utility)
            }
        }
    }
}

fn scalar_variance_and_error(m2: f64, sample_count: usize) -> (f64, f64) {
    if sample_count < 2 {
        return (0.0, 0.0);
    }
    let variance = (m2 / (sample_count - 1) as f64).max(0.0);
    (variance, (variance / sample_count as f64).sqrt())
}

fn estimate_variance_and_error(m2: &[f64], sample_count: usize) -> (Vec<f64>, Vec<f64>) {
    let mut variance = Vec::with_capacity(m2.len());
    let mut standard_error = Vec::with_capacity(m2.len());
    for &value in m2 {
        let (sample_variance, sample_error) = scalar_variance_and_error(value, sample_count);
        variance.push(sample_variance);
        standard_error.push(sample_error);
    }
    (variance, standard_error)
}

fn update_online_moment(mean: &mut f64, m2: &mut f64, value: f64, count: f64) {
    let delta = value - *mean;
    *mean += delta / count;
    let second_delta = value - *mean;
    *m2 += delta * second_delta;
}

fn execute_parallel_batch_task(
    tree: &GameTree,
    sampler: &MultiwayPrivateDealSampler,
    snapshot: &HashMap<BatchInfoSetKey, BatchInfoSetData>,
    player_count: usize,
    max_private_attempts: usize,
    task: ParallelBatchTask,
) -> Result<ParallelBatchTaskResult, String> {
    let mut private_sampler = sampler.fork_with_state(task.private_seed, 0)?;
    let sample = private_sampler.sample(max_private_attempts)?;
    let profile = MultiwayHoldemPublicProfile {
        payoff: MultiwayHoldemChipEvPayoff::new(sample.hands.clone())?,
        hands: sample.hands,
    };
    let mut traversal_rng = XorShift64::new(task.traversal_seed);
    let mut sampled_chance_nodes = 0;
    let mut sampled_opponent_actions = 0;
    let mut infoset_visits_by_player = vec![0u64; player_count];
    let mut deltas = HashMap::<BatchInfoSetKey, BatchInfoSetDelta>::new();
    traverse_parallel_public_profile(
        tree,
        &profile,
        tree.root,
        task.traverser,
        vec![1.0; player_count],
        snapshot,
        &mut traversal_rng,
        &mut sampled_chance_nodes,
        &mut sampled_opponent_actions,
        &mut infoset_visits_by_player,
        &mut deltas,
    )?;
    let mut updates: Vec<_> = deltas.into_values().collect();
    updates.sort_by_key(|update| update.key);
    Ok(ParallelBatchTaskResult {
        traverser: task.traverser,
        private_sampling_attempts: sample.attempts,
        sampled_chance_nodes,
        sampled_opponent_actions,
        infoset_visits_by_player,
        updates,
    })
}

fn traverse_parallel_public_profile(
    tree: &GameTree,
    profile: &MultiwayHoldemPublicProfile,
    node_id: TreeNodeId,
    traverser: PlayerId,
    reach: Vec<f64>,
    snapshot: &HashMap<BatchInfoSetKey, BatchInfoSetData>,
    traversal_rng: &mut XorShift64,
    sampled_chance_nodes: &mut u64,
    sampled_opponent_actions: &mut u64,
    infoset_visits_by_player: &mut [u64],
    deltas: &mut HashMap<BatchInfoSetKey, BatchInfoSetDelta>,
) -> Result<f64, String> {
    let node = tree
        .node(node_id)
        .ok_or_else(|| format!("unknown public node {node_id}"))?
        .clone();
    if let Some(terminal) = &node.leaf {
        if matches!(terminal, LeafKind::RoundComplete) {
            return Err(format!(
                "round-complete public node {node_id} has no payoff"
            ));
        }
        return Ok(profile.terminal_utility(&node)?[traverser]);
    }
    if node.chance.is_some() {
        let outcomes = profile.chance_outcomes(tree, node_id)?;
        let probabilities: Vec<f64> = outcomes
            .iter()
            .map(|(probability, _)| *probability)
            .collect();
        let selected = traversal_rng.sample_index(&probabilities);
        *sampled_chance_nodes += 1;
        return traverse_parallel_public_profile(
            tree,
            profile,
            outcomes[selected].1,
            traverser,
            reach,
            snapshot,
            traversal_rng,
            sampled_chance_nodes,
            sampled_opponent_actions,
            infoset_visits_by_player,
            deltas,
        );
    }

    let player = node
        .state
        .actor
        .ok_or_else(|| format!("public decision node {node_id} has no actor"))?;
    if player >= profile.hands().len() || player >= reach.len() {
        return Err(format!(
            "public decision player {player} is outside traversal state"
        ));
    }
    let action_count = node.children.len();
    if action_count == 0 {
        return Err(format!("public decision node {node_id} has no actions"));
    }
    let key = BatchInfoSetKey {
        player,
        public_node: node_id,
        private_hand: profile.hands()[player],
    };
    let strategy = if let Some(entry) = snapshot.get(&key) {
        if entry.action_count != action_count || entry.data.regret_sum.len() != action_count {
            return Err(format!(
                "parallel information set {player}:{node_id} changes action count"
            ));
        }
        entry.data.current_strategy()
    } else {
        vec![1.0 / action_count as f64; action_count]
    };
    if strategy.len() != action_count {
        return Err(format!(
            "parallel strategy action count mismatch at node {node_id}"
        ));
    }

    if player != traverser {
        let action = traversal_rng.sample_index(&strategy);
        *sampled_opponent_actions += 1;
        let mut child_reach = reach;
        child_reach[player] *= strategy[action];
        return traverse_parallel_public_profile(
            tree,
            profile,
            node.children[action],
            traverser,
            child_reach,
            snapshot,
            traversal_rng,
            sampled_chance_nodes,
            sampled_opponent_actions,
            infoset_visits_by_player,
            deltas,
        );
    }

    let mut action_values = Vec::with_capacity(action_count);
    for (action, &child) in node.children.iter().enumerate() {
        let mut child_reach = reach.clone();
        child_reach[player] *= strategy[action];
        action_values.push(traverse_parallel_public_profile(
            tree,
            profile,
            child,
            traverser,
            child_reach,
            snapshot,
            traversal_rng,
            sampled_chance_nodes,
            sampled_opponent_actions,
            infoset_visits_by_player,
            deltas,
        )?);
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
    let update = deltas.entry(key).or_insert_with(|| BatchInfoSetDelta {
        key,
        action_count,
        regret_delta: vec![0.0; action_count],
        strategy_delta: vec![0.0; action_count],
        visits: 0,
    });
    for action in 0..action_count {
        let regret_delta = counterfactual_reach * (action_values[action] - node_value);
        let strategy_delta = strategy_reach * strategy[action];
        if !regret_delta.is_finite() || !strategy_delta.is_finite() {
            return Err("parallel batch produced non-finite regret update".to_string());
        }
        update.regret_delta[action] += regret_delta;
        update.strategy_delta[action] += strategy_delta;
    }
    update.visits += 1;
    infoset_visits_by_player[player] += 1;
    Ok(node_value)
}

fn public_profile_key(
    tree: &GameTree,
    profile: &MultiwayHoldemPublicProfile,
    node_id: TreeNodeId,
    player: PlayerId,
    action_count: usize,
    infosets: &mut HashMap<BatchInfoSetKey, BatchInfoSetData>,
) -> Result<BatchInfoSetKey, String> {
    let _ = tree
        .node(node_id)
        .ok_or_else(|| format!("unknown public node {node_id}"))?;
    let private_hand = *profile
        .hands()
        .get(player)
        .ok_or_else(|| format!("player {player} is outside private profile"))?;
    let key = BatchInfoSetKey {
        player,
        public_node: node_id,
        private_hand,
    };
    let entry = infosets.entry(key).or_insert_with(|| BatchInfoSetData {
        action_count,
        data: InfoSetData::new(action_count),
    });
    if entry.action_count != action_count || entry.data.regret_sum.len() != action_count {
        return Err(format!(
            "public batch information set {player}:{node_id} changes action count"
        ));
    }
    Ok(key)
}

fn traverse_public_profile(
    tree: &GameTree,
    profile: &MultiwayHoldemPublicProfile,
    node_id: TreeNodeId,
    traverser: PlayerId,
    reach: Vec<f64>,
    infosets: &mut HashMap<BatchInfoSetKey, BatchInfoSetData>,
    traversal_rng: &mut XorShift64,
    metrics: &mut MultiwayBatchSamplingMetrics,
) -> Result<f64, String> {
    let node = tree
        .node(node_id)
        .ok_or_else(|| format!("unknown public node {node_id}"))?
        .clone();
    if let Some(terminal) = &node.leaf {
        if matches!(terminal, LeafKind::RoundComplete) {
            return Err(format!(
                "round-complete public node {node_id} has no payoff"
            ));
        }
        return Ok(profile.terminal_utility(&node)?[traverser]);
    }
    if node.chance.is_some() {
        let outcomes = profile.chance_outcomes(tree, node_id)?;
        let probabilities: Vec<f64> = outcomes
            .iter()
            .map(|(probability, _)| *probability)
            .collect();
        let selected = traversal_rng.sample_index(&probabilities);
        metrics.sampled_chance_nodes += 1;
        return traverse_public_profile(
            tree,
            profile,
            outcomes[selected].1,
            traverser,
            reach,
            infosets,
            traversal_rng,
            metrics,
        );
    }

    let player = node
        .state
        .actor
        .ok_or_else(|| format!("public decision node {node_id} has no actor"))?;
    let action_count = node.children.len();
    if action_count == 0 {
        return Err(format!("public decision node {node_id} has no actions"));
    }
    let key = public_profile_key(tree, profile, node_id, player, action_count, infosets)?;
    let strategy = infosets
        .get(&key)
        .ok_or_else(|| "public batch information set disappeared".to_string())?
        .data
        .current_strategy();

    if player != traverser {
        let action = traversal_rng.sample_index(&strategy);
        metrics.sampled_opponent_actions += 1;
        let mut child_reach = reach;
        child_reach[player] *= strategy[action];
        return traverse_public_profile(
            tree,
            profile,
            node.children[action],
            traverser,
            child_reach,
            infosets,
            traversal_rng,
            metrics,
        );
    }

    let mut action_values = Vec::with_capacity(action_count);
    for (action, &child) in node.children.iter().enumerate() {
        let mut child_reach = reach.clone();
        child_reach[player] *= strategy[action];
        action_values.push(traverse_public_profile(
            tree,
            profile,
            child,
            traverser,
            child_reach,
            infosets,
            traversal_rng,
            metrics,
        )?);
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
    let data = infosets
        .get_mut(&key)
        .ok_or_else(|| "public batch information set disappeared during update".to_string())?;
    data.data.visits += 1;
    metrics.infoset_visits += 1;
    metrics.infoset_visits_by_player[player] += 1;
    for action in 0..action_count {
        let regret_delta = counterfactual_reach * (action_values[action] - node_value);
        if !regret_delta.is_finite() {
            return Err("public batch MCCFR produced non-finite regret".to_string());
        }
        data.data.regret_sum[action] = (data.data.regret_sum[action] + regret_delta).max(0.0);
        data.data.strategy_sum[action] += strategy_reach * strategy[action];
    }
    Ok(node_value)
}

fn evaluate_public_profile(
    tree: &GameTree,
    profile: &MultiwayHoldemPublicProfile,
    infosets: &HashMap<BatchInfoSetKey, BatchInfoSetData>,
) -> Result<Vec<f64>, String> {
    evaluate_public_node(tree, profile, tree.root, infosets)
}

fn evaluate_public_node(
    tree: &GameTree,
    profile: &MultiwayHoldemPublicProfile,
    node_id: TreeNodeId,
    infosets: &HashMap<BatchInfoSetKey, BatchInfoSetData>,
) -> Result<Vec<f64>, String> {
    let node = tree
        .node(node_id)
        .ok_or_else(|| format!("unknown public node {node_id}"))?;
    if let Some(terminal) = &node.leaf {
        if matches!(terminal, LeafKind::RoundComplete) {
            return Err(format!(
                "round-complete public node {node_id} has no payoff"
            ));
        }
        return profile.terminal_utility(node);
    }
    if node.chance.is_some() {
        let outcomes = profile.chance_outcomes(tree, node_id)?;
        let mut utility = vec![0.0; profile.hands().len()];
        for (probability, child) in outcomes {
            let child_utility = evaluate_public_node(tree, profile, child, infosets)?;
            for (total, value) in utility.iter_mut().zip(child_utility) {
                *total += probability * value;
            }
        }
        return Ok(utility);
    }

    let player = node
        .state
        .actor
        .ok_or_else(|| format!("public decision node {node_id} has no actor"))?;
    let key = BatchInfoSetKey {
        player,
        public_node: node_id,
        private_hand: profile.hands()[player],
    };
    let strategy = infosets
        .get(&key)
        .map(|entry| entry.data.average_strategy())
        .unwrap_or_else(|| vec![1.0 / node.children.len() as f64; node.children.len()]);
    if strategy.len() != node.children.len() {
        return Err(format!(
            "public strategy action count mismatch at node {node_id}"
        ));
    }
    let mut utility = vec![0.0; profile.hands().len()];
    for (&probability, &child) in strategy.iter().zip(&node.children) {
        let child_utility = evaluate_public_node(tree, profile, child, infosets)?;
        for (total, value) in utility.iter_mut().zip(child_utility) {
            *total += probability * value;
        }
    }
    Ok(utility)
}

fn evaluate_public_best_response(
    tree: &GameTree,
    profile: &MultiwayHoldemPublicProfile,
    infosets: &HashMap<BatchInfoSetKey, BatchInfoSetData>,
    target_player: PlayerId,
    node_id: TreeNodeId,
) -> Result<f64, String> {
    if target_player >= profile.hands().len() {
        return Err(format!(
            "best-response target player {target_player} is outside private profile"
        ));
    }
    let node = tree
        .node(node_id)
        .ok_or_else(|| format!("unknown public node {node_id}"))?;
    if let Some(terminal) = &node.leaf {
        if matches!(terminal, LeafKind::RoundComplete) {
            return Err(format!(
                "round-complete public node {node_id} has no payoff"
            ));
        }
        return Ok(profile.terminal_utility(node)?[target_player]);
    }
    if node.chance.is_some() {
        let outcomes = profile.chance_outcomes(tree, node_id)?;
        let mut value = 0.0;
        for (probability, child) in outcomes {
            value += probability
                * evaluate_public_best_response(tree, profile, infosets, target_player, child)?;
        }
        return Ok(value);
    }

    let player = node
        .state
        .actor
        .ok_or_else(|| format!("public decision node {node_id} has no actor"))?;
    if node.children.is_empty() {
        return Err(format!("public decision node {node_id} has no actions"));
    }
    if player == target_player {
        let mut best = f64::NEG_INFINITY;
        for &child in &node.children {
            best = best.max(evaluate_public_best_response(
                tree,
                profile,
                infosets,
                target_player,
                child,
            )?);
        }
        return Ok(best);
    }

    if player >= profile.hands().len() {
        return Err(format!(
            "public decision player {player} is outside private profile"
        ));
    }
    let key = BatchInfoSetKey {
        player,
        public_node: node_id,
        private_hand: profile.hands()[player],
    };
    let strategy = infosets
        .get(&key)
        .map(|entry| entry.data.average_strategy())
        .unwrap_or_else(|| vec![1.0 / node.children.len() as f64; node.children.len()]);
    if strategy.len() != node.children.len() {
        return Err(format!(
            "best-response opponent strategy action count mismatch at node {node_id}"
        ));
    }
    let mut value = 0.0;
    for (&probability, &child) in strategy.iter().zip(&node.children) {
        value += probability
            * evaluate_public_best_response(tree, profile, infosets, target_player, child)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_cards::cards_from_str;
    use holdem_domain::{GameState, PlayerState, Street, TerminalState};
    use holdem_tree::TreeNode;

    fn combo(text: &str) -> Combo {
        let cards = cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    fn ranges() -> Vec<WeightedRange> {
        [["As Ah", "Kc Kd"], ["Jc Js", "Qs Qh"], ["Tc Td", "8c 8d"]]
            .into_iter()
            .map(|entries| WeightedRange {
                combos: entries
                    .into_iter()
                    .map(|cards| {
                        let combo = combo(cards);
                        holdem_ranges::WeightedCombo {
                            combo,
                            class_id: combo.class_id(),
                            weight: 1.0,
                        }
                    })
                    .collect(),
            })
            .collect()
    }

    fn tiny_three_way_tree() -> GameTree {
        let mut players = vec![
            PlayerState::new(0, 900).unwrap(),
            PlayerState::new(1, 900).unwrap(),
            PlayerState::new(2, 900).unwrap(),
        ];
        for player in &mut players {
            player.committed_total = 100;
            player.committed_street = 100;
        }
        let mut root_state = GameState::new(
            3,
            Street::Flop,
            cards_from_str("2s 7d 9c").unwrap(),
            players,
            0,
        )
        .unwrap();
        root_state.configure_betting(0, 100, 100, vec![0]).unwrap();

        let mut fold_state = root_state.clone();
        fold_state.actor = None;
        fold_state.pending_players.clear();
        fold_state.raise_allowed.clear();
        fold_state.terminal = Some(TerminalState::Fold { winner: 1 });

        let mut showdown_state = root_state.clone();
        showdown_state.actor = None;
        showdown_state.pending_players.clear();
        showdown_state.raise_allowed.clear();
        showdown_state.terminal = Some(TerminalState::Showdown);

        GameTree {
            root: 0,
            nodes: vec![
                TreeNode {
                    id: 0,
                    parent: None,
                    action_from_parent: None,
                    state: root_state,
                    children: vec![1, 2],
                    leaf: None,
                    chance: None,
                },
                TreeNode {
                    id: 1,
                    parent: Some(0),
                    action_from_parent: Some(Action::Fold),
                    state: fold_state,
                    children: Vec::new(),
                    leaf: Some(LeafKind::Terminal(TerminalState::Fold { winner: 1 })),
                    chance: None,
                },
                TreeNode {
                    id: 2,
                    parent: Some(0),
                    action_from_parent: Some(Action::Call),
                    state: showdown_state,
                    children: Vec::new(),
                    leaf: Some(LeafKind::Terminal(TerminalState::Showdown)),
                    chance: None,
                },
            ],
        }
    }

    #[test]
    fn tree_fingerprint_changes_with_public_tree_shape() {
        let first = GameTree {
            root: 0,
            nodes: Vec::new(),
        };
        let second = GameTree {
            root: 1,
            nodes: Vec::new(),
        };
        assert_ne!(
            multiway_holdem_tree_fingerprint(&first),
            multiway_holdem_tree_fingerprint(&second)
        );
    }

    #[test]
    fn sampled_batch_shares_infosets_and_resumes_deterministically() {
        let tree = tiny_three_way_tree();
        let range_values = ranges();
        let mut one_shot = MultiwayHoldemBatchSolver::new_with_profile_cache_capacity(
            tree.clone(),
            range_values.clone(),
            0,
            123,
            777,
            100,
            8,
        )
        .unwrap();
        one_shot.run(6).unwrap();
        let cache_metrics = one_shot.compiled_profile_cache_metrics();
        assert!(cache_metrics.misses > 0);
        assert_eq!(
            cache_metrics.hits + cache_metrics.misses,
            one_shot.metrics().sampled_private_deals
        );

        let mut split = MultiwayHoldemBatchSolver::new_with_profile_cache_capacity(
            tree.clone(),
            range_values.clone(),
            0,
            123,
            777,
            100,
            8,
        )
        .unwrap();
        split.run(3).unwrap();
        let checkpoint = split.checkpoint();
        assert!(checkpoint.metrics.sampled_private_deals > 0);
        assert!(checkpoint.metrics.infoset_visits > 0);
        let mut resumed = MultiwayHoldemBatchSolver::from_checkpoint(
            tree,
            range_values,
            0,
            &checkpoint,
            777,
            100,
        )
        .unwrap();
        resumed.run(3).unwrap();

        assert_eq!(resumed.iterations(), one_shot.iterations());
        assert_eq!(resumed.infoset_count(), one_shot.infoset_count());
        assert_eq!(
            resumed.metrics().traverser_updates_by_player,
            one_shot.metrics().traverser_updates_by_player
        );
        assert_eq!(
            resumed.metrics().infoset_visits_by_player,
            one_shot.metrics().infoset_visits_by_player
        );
        let strategy = resumed.average_strategy(0, 0, combo("As Ah")).unwrap();
        assert!((strategy.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert_eq!(
            resumed.checkpoint().traversal_rng_state,
            one_shot.checkpoint().traversal_rng_state
        );
        assert_eq!(
            resumed.checkpoint().private_rng_state,
            one_shot.checkpoint().private_rng_state
        );
        for (left, right) in resumed
            .checkpoint()
            .infosets
            .iter()
            .zip(one_shot.checkpoint().infosets.iter())
        {
            assert_eq!(
                (left.player, left.public_node),
                (right.player, right.public_node)
            );
            assert_eq!(left.private_cards, right.private_cards);
            for (left_value, right_value) in left.regret_sum.iter().zip(right.regret_sum.iter()) {
                assert!((left_value - right_value).abs() < 1e-12);
            }
        }
        let utility = resumed.evaluate_average_utility(4, 100).unwrap();
        assert_eq!(utility.len(), 3);
        assert!(utility.iter().all(|value| value.is_finite()));
        let report = resumed.strategy_report(4, 100).unwrap();
        assert_eq!(report.schema_version, MULTIWAY_BATCH_RESULT_SCHEMA_VERSION);
        assert!(!report.infosets.is_empty());
        assert!(report.infosets.iter().all(|infoset| {
            (infoset
                .actions
                .iter()
                .map(|action| action.frequency)
                .sum::<f64>()
                - 1.0)
                .abs()
                < 1e-9
        }));
        let report_json = report.to_json().unwrap();
        assert!(report_json.contains("multiway_batch_strategy_report"));
        assert!(report_json.contains("utility_estimate"));
        assert!(report_json.contains("convergence"));
        assert!(report
            .to_csv()
            .unwrap()
            .starts_with("schema_version,format"));
    }

    #[test]
    fn parallel_batch_is_deterministic_across_worker_counts_and_resume() {
        let tree = tiny_three_way_tree();
        let range_values = ranges();
        let mut one_worker =
            MultiwayHoldemBatchSolver::new(tree.clone(), range_values.clone(), 0, 991, 444, 100)
                .unwrap();
        one_worker.run_parallel(5, 1, 2).unwrap();

        let mut many_workers =
            MultiwayHoldemBatchSolver::new(tree.clone(), range_values.clone(), 0, 991, 444, 100)
                .unwrap();
        many_workers.run_parallel(5, 3, 2).unwrap();
        assert_eq!(many_workers.checkpoint(), one_worker.checkpoint());

        let mut split =
            MultiwayHoldemBatchSolver::new(tree.clone(), range_values.clone(), 0, 991, 444, 100)
                .unwrap();
        split.run_parallel(2, 2, 2).unwrap();
        split.run_parallel(3, 2, 2).unwrap();
        assert_eq!(split.checkpoint(), one_worker.checkpoint());

        let mut cached = MultiwayHoldemBatchSolver::new_with_profile_cache_capacity(
            tree,
            range_values,
            0,
            991,
            444,
            100,
            4,
        )
        .unwrap();
        let error = cached.run_parallel(1, 2, 1).unwrap_err();
        assert!(error.contains("public-tree arena"));
    }

    #[test]
    fn persistent_job_store_rotates_checkpoints_and_restores_latest_state() {
        let tree = tiny_three_way_tree();
        let range_values = ranges();
        let directory = std::env::temp_dir().join(format!(
            "holdem-solver-job-{}-{}",
            std::process::id(),
            991u64
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let store = crate::MultiwayBatchJobStore::new(&directory).unwrap();
        let config = crate::MultiwayBatchJobConfig {
            target_iterations: 5,
            worker_count: 2,
            reduction_batch_size: 2,
            checkpoint_interval: 2,
            max_private_attempts: 100,
            keep_checkpoints: 2,
        };
        let mut solver =
            MultiwayHoldemBatchSolver::new(tree.clone(), range_values.clone(), 0, 991, 444, 100)
                .unwrap();
        let manifest = store
            .run_to_target("job-test", &mut solver, &config)
            .unwrap();
        assert_eq!(manifest.status, crate::MultiwayBatchJobStatus::Completed);
        assert_eq!(manifest.completed_iterations, 5);
        assert!(manifest.latest_checkpoint.is_some());
        let checkpoint_files = std::fs::read_dir(store.checkpoints_directory())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .count();
        assert_eq!(checkpoint_files, 2);

        let resumed = store
            .resume_solver(tree.clone(), range_values.clone(), 0, 444)
            .unwrap();
        assert_eq!(resumed.iterations(), 5);
        let resumed_checkpoint = resumed.checkpoint();
        let source_checkpoint = solver.checkpoint();
        assert_eq!(
            resumed_checkpoint.traversal_rng_state,
            source_checkpoint.traversal_rng_state
        );
        assert_eq!(
            resumed_checkpoint.private_rng_state,
            source_checkpoint.private_rng_state
        );
        assert_eq!(
            resumed_checkpoint.infosets.len(),
            source_checkpoint.infosets.len()
        );
        for (left, right) in resumed_checkpoint
            .infosets
            .iter()
            .zip(source_checkpoint.infosets.iter())
        {
            assert_eq!(
                (left.player, left.public_node),
                (right.player, right.public_node)
            );
            assert_eq!(left.private_cards, right.private_cards);
            assert_eq!(left.visits, right.visits);
            for (left_value, right_value) in left
                .regret_sum
                .iter()
                .chain(left.strategy_sum.iter())
                .zip(right.regret_sum.iter().chain(right.strategy_sum.iter()))
            {
                assert!((left_value - right_value).abs() < 1e-12);
            }
        }

        let extended_config = crate::MultiwayBatchJobConfig {
            target_iterations: 7,
            ..config.clone()
        };
        let mut extended = store.resume_solver(tree, range_values, 0, 444).unwrap();
        let extended_manifest = store
            .run_to_target("job-test", &mut extended, &extended_config)
            .unwrap();
        assert_eq!(extended_manifest.completed_iterations, 7);
        assert_eq!(extended_manifest.config.target_iterations, 7);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn persistent_job_store_supports_cooperative_cancellation_and_resume() {
        let tree = tiny_three_way_tree();
        let range_values = ranges();
        let directory = std::env::temp_dir().join(format!(
            "holdem-solver-cancel-job-{}-{}",
            std::process::id(),
            992u64
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let store = crate::MultiwayBatchJobStore::new(&directory).unwrap();
        let config = crate::MultiwayBatchJobConfig {
            target_iterations: 5,
            worker_count: 1,
            reduction_batch_size: 1,
            checkpoint_interval: 2,
            max_private_attempts: 100,
            keep_checkpoints: 2,
        };
        let mut solver =
            MultiwayHoldemBatchSolver::new(tree.clone(), range_values.clone(), 0, 771, 662, 100)
                .unwrap();
        let mut checks = 0;
        let cancelled = store
            .run_to_target_with_control("cancel-test", &mut solver, &config, || {
                checks += 1;
                checks <= 2
            })
            .unwrap();
        assert_eq!(cancelled.status, crate::MultiwayBatchJobStatus::Cancelled);
        assert_eq!(cancelled.completed_iterations, 2);

        let mut resumed = store.resume_solver(tree, range_values, 0, 662).unwrap();
        assert_eq!(resumed.iterations(), 2);
        let completed = store
            .run_to_target("cancel-test", &mut resumed, &config)
            .unwrap();
        assert_eq!(completed.status, crate::MultiwayBatchJobStatus::Completed);
        assert_eq!(completed.completed_iterations, 5);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn default_batch_solver_uses_public_arena_without_profile_compilation() {
        let tree = tiny_three_way_tree();
        let range_values = ranges();
        let mut solver =
            MultiwayHoldemBatchSolver::new(tree.clone(), range_values.clone(), 0, 321, 888, 100)
                .unwrap();
        solver.run(4).unwrap();
        let cache_metrics = solver.compiled_profile_cache_metrics();
        assert_eq!(cache_metrics.hits, 0);
        assert_eq!(cache_metrics.misses, 0);
        assert!(solver.infoset_count() > 0);
        let metrics = solver.metrics();
        assert_eq!(metrics.traverser_updates, 12);
        assert_eq!(metrics.traverser_updates_by_player, vec![4, 4, 4]);
        assert_eq!(metrics.sampled_private_deals_by_player, vec![4, 4, 4]);
        assert_eq!(
            metrics.infoset_visits_by_player.iter().sum::<u64>(),
            metrics.infoset_visits
        );
        let utility_estimate = solver.evaluate_average_utility_estimate(8, 100).unwrap();
        assert_eq!(utility_estimate.samples, 8);
        assert_eq!(utility_estimate.mean.len(), 3);
        assert!(utility_estimate
            .variance
            .iter()
            .chain(utility_estimate.standard_error.iter())
            .all(|value| value.is_finite() && *value >= 0.0));
        let diagnostics = solver.convergence_diagnostics();
        assert_eq!(diagnostics.players.len(), 3);
        assert_eq!(diagnostics.infosets, solver.infoset_count() as u64);
        assert!(diagnostics.positive_regret_sum.is_finite());
        let best_response = solver.best_response_probe(4, 100).unwrap();
        assert_eq!(best_response.players.len(), 3);
        assert!(best_response.players.iter().all(|estimate| {
            estimate.best_response_value + 1e-9 >= estimate.strategy_value
                && estimate.improvement_standard_error.is_finite()
        }));
        let report = solver.strategy_report(2, 100).unwrap();
        assert_eq!(report.player_count, 3);
        assert_eq!(report.utility_estimate.samples, 2);
        assert_eq!(report.convergence.players.len(), 3);
        assert!(!report.infosets.is_empty());
        let checkpoint = solver.checkpoint();
        assert_eq!(checkpoint.profile_cache_capacity, 0);
        let mut resumed = MultiwayHoldemBatchSolver::from_checkpoint(
            tree,
            range_values,
            0,
            &checkpoint,
            888,
            100,
        )
        .unwrap();
        resumed.run(1).unwrap();
        assert_eq!(resumed.iterations(), solver.iterations() + 1);
    }
}
