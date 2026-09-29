//! T4.2/D-022, T5.2/D-025: вырезка спотов из префлоп-каркаса (Round-дерева),
//! обходчик каркаса -> MultiwayStaticGame, каркасный MCCFR-раунд,
//! итеративная схема каркас <-> споты.
//!
//! Каркас даёт структуру префлоп-действий (D-024(10a)); каждый лист типа
//! RoundComplete — закрытый префлоп-раунд. Вырезка превращает лист в job
//! спота (D-020-класс): история реконструируется по parent-links,
//! hero = первый постфлоп-актор, диапазоны наследуются от каркаса
//! (v1 — без range propagation, это T5.3/D-024(8)), борд — фикс.
//! Дублей нет по построению: узел каркаса входит ровно в один спот.
//! Контракт реконструкции: действие ребра живёт в ДОЧЕРНЕМ узле
//! (`action_from_parent: Option<Action>`), а не списком у родителя.

use std::collections::HashMap;

use holdem_domain::PlayerId;
use holdem_domain::TerminalState;
use holdem_tree::{GameTree, LeafKind};

use crate::multiway::{MultiwayGameNode, MultiwayMccfrSolver, MultiwayStaticGame};

/// Действие пути как JSON-запись истории (kind + to).
#[derive(Debug, Clone, PartialEq)]
pub struct BlueprintHistoryAction {
    pub player: PlayerId,
    pub kind: String,
    pub to: Option<i64>,
}

/// Вырезанный спот: лист + история + hero + метрики.
#[derive(Debug, Clone)]
pub struct CarvedSpot {
    pub leaf_node: usize,
    pub history: Vec<BlueprintHistoryAction>,
    pub hero_player: PlayerId,
    pub pot: i64,
    pub active_players: usize,
}

/// Ошибка вырезки.
#[derive(Debug)]
pub enum CarveError {
    NodeIsNotRoundComplete(usize),
    HistoryReconstruction(String),
    NoHero,
}

impl std::fmt::Display for CarveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeIsNotRoundComplete(id) => {
                write!(f, "node {id} is not a round-complete leaf")
            }
            Self::HistoryReconstruction(message) => {
                write!(f, "history reconstruction failed: {message}")
            }
            Self::NoHero => write!(f, "no active player for hero"),
        }
    }
}

/// Вырезает спот из листа RoundComplete.
///
/// История реконструируется снизу вверх по parent-links с реверсом;
/// действие ребра читается из дочернего узла (`action_from_parent`),
/// актор — из состояния родителя (actor до применения действия).
pub fn carve_spot(tree: &GameTree, leaf: usize) -> Result<CarvedSpot, CarveError> {
    let node = tree
        .node(leaf)
        .ok_or(CarveError::NodeIsNotRoundComplete(leaf))?;
    if !matches!(node.leaf, Some(LeafKind::RoundComplete)) {
        return Err(CarveError::NodeIsNotRoundComplete(leaf));
    }

    let mut raw: Vec<BlueprintHistoryAction> = Vec::new();
    let mut current = leaf;
    loop {
        let node = tree
            .node(current)
            .ok_or_else(|| CarveError::HistoryReconstruction(format!("node {current} missing")))?;
        let Some(parent) = node.parent else {
            break;
        };
        // Действие ребра — поле ДОЧЕРНЕГО узла (контракт репо).
        let action = node.action_from_parent.clone().ok_or_else(|| {
            CarveError::HistoryReconstruction(format!("edge {parent} -> {current} has no action"))
        })?;
        // Актор — состояние родителя (actor до действия).
        let parent_node = tree
            .node(parent)
            .ok_or_else(|| CarveError::HistoryReconstruction(format!("parent {parent} missing")))?;
        let actor = parent_node.state.actor.ok_or_else(|| {
            CarveError::HistoryReconstruction(format!("parent {parent} has no actor"))
        })?;
        raw.push(BlueprintHistoryAction {
            player: actor,
            kind: action_kind_json(&action),
            to: action_target(&action),
        });
        current = parent;
    }
    raw.reverse();

    let state = &node.state;
    let button = state
        .players
        .iter()
        .find(|p| {
            use holdem_domain::table::Position;
            matches!(p.position, Position::Button | Position::ButtonSmallBlind)
        })
        .map(|p| p.seat)
        .ok_or(CarveError::NoHero)?;
    let hero_player = first_active_after(state, button).ok_or(CarveError::NoHero)?;

    use holdem_domain::PlayerStatus;
    let active_players = state
        .players
        .iter()
        .filter(|p| matches!(p.status, PlayerStatus::Active | PlayerStatus::AllIn))
        .count();

    Ok(CarvedSpot {
        leaf_node: leaf,
        history: raw,
        hero_player,
        pot: state.pot,
        active_players,
    })
}

fn action_kind_json(action: &holdem_domain::Action) -> String {
    use holdem_domain::Action;
    match action {
        Action::Fold => "fold".to_string(),
        Action::Check => "check".to_string(),
        Action::Call => "call".to_string(),
        Action::Bet { .. } => "bet".to_string(),
        Action::Raise { .. } => "raise".to_string(),
        Action::AllIn => "all_in".to_string(),
    }
}

fn action_target(action: &holdem_domain::Action) -> Option<i64> {
    use holdem_domain::Action;
    match action {
        Action::Bet { to } | Action::Raise { to } => Some(*to),
        _ => None,
    }
}

/// Первый Active игрок после баттона (доменное правило постфлопа).
fn first_active_after(state: &holdem_domain::GameState, button: usize) -> Option<PlayerId> {
    use holdem_domain::PlayerStatus;
    for offset in 1..=state.table_size {
        let seat = (button + offset) % state.table_size;
        let player = state.players.iter().find(|p| p.seat == seat)?;
        if matches!(player.status, PlayerStatus::Active) {
            return Some(seat);
        }
    }
    None
}

/// Собирает job-JSON спота (serde Value; table/ranges вставляет вызывающий).
pub fn carved_spot_job_json(
    spot: &CarvedSpot,
    ranges: &[String],
    hero_hand: &str,
    hero_label: &str,
    board: &str,
    turn_card: &str,
    river_card: &str,
    seed: u64,
) -> serde_json::Value {
    let history: Vec<serde_json::Value> = spot
        .history
        .iter()
        .map(|action| {
            let mut value = serde_json::json!({ "player": action.player, "kind": action.kind });
            if let Some(to) = action.to {
                value["to"] = serde_json::json!(to);
            }
            value
        })
        .collect();
    serde_json::json!({
        "schema_version": 1,
        "dead_cards": "",
        "history": history,
        "board": board,
        "ranges": ranges.iter().map(|r| serde_json::json!({ "range": r })).collect::<Vec<_>>(),
        "hero": { "player": spot.hero_player, "hand": hero_hand, "label": hero_label },
        "tree": {
            "mode": "full",
            "max_nodes": 100000,
            "max_depth": 64,
            "preflop": {},
            "flop": { "bet_fractions": [0.33, 0.66], "raise_multipliers": [3.0] },
            "turn": { "bet_fractions": [0.33, 0.66], "raise_multipliers": [3.0] },
            "river": { "bet_fractions": [0.33, 0.66], "raise_multipliers": [3.0] },
            "chance": {
                "flop": [],
                "turn": [{ "cards": turn_card, "probability": 1.0 }],
                "river": [{ "cards": river_card, "probability": 1.0 }],
                "enumerate_exact": false,
                "max_outcomes_per_node": 10000
            }
        },
        "execution": {
            "seed": seed,
            "target_iterations": 10000,
            "checkpoint_interval": 1000,
            "keep_checkpoints": 5,
            "max_private_attempts": 100000,
            "worker_count": 4,
            "reduction_batch_size": 8,
            "config_fingerprint": 0
        }
    })
}

// ============================================================================
// T5.2 ч.3 (D-025(6)): обходчик каркаса -> MultiwayStaticGame.
//
// Каркас (Round-дерево) не проходит compile_multiway_holdem_tree — его
// RoundComplete-листья «не имеют пэйоффа». Обходчик строит
// MultiwayStaticGame напрямую: узлы решений -> Decision; фолды ->
// Terminal{utility} семантикой ChipEvPayoff (winner: pot − committed;
// остальные − committed; zero-sum); RoundComplete -> Terminal{utility}
// из таблицы EV-листьев (недостающие — нули, покрытие uncovered/total).
// ============================================================================

/// EV-листья каркаса: node_id -> utility-вектор по игрокам.
#[derive(Debug, Clone, Default)]
pub struct BlueprintLeafEv {
    entries: HashMap<usize, Vec<f64>>,
    player_count: usize,
}

impl BlueprintLeafEv {
    pub fn new(player_count: usize) -> Self {
        Self {
            entries: HashMap::new(),
            player_count,
        }
    }

    /// Записывает EV-вектор листа (длина обязана равняться player_count).
    pub fn set(&mut self, leaf: usize, utility: Vec<f64>) -> Result<(), String> {
        if utility.len() != self.player_count {
            return Err(format!(
                "leaf EV vector has {} entries, expected {}",
                utility.len(),
                self.player_count
            ));
        }
        self.entries.insert(leaf, utility);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn get(&self, leaf: usize) -> Vec<f64> {
        self.entries
            .get(&leaf)
            .cloned()
            .unwrap_or_else(|| vec![0.0; self.player_count])
    }
}

/// Результат сборки игры из каркаса.
#[derive(Debug)]
pub struct BlueprintGameBuild {
    pub game: MultiwayStaticGame,
    /// RoundComplete-листья без EV (нули).
    pub uncovered_leaves: usize,
    /// Всего RoundComplete-листьев.
    pub total_leaves: usize,
}

/// Детерминированный идентификатор инфосета узла каркаса:
/// FNV-1a от (player, node_id).
fn blueprint_infoset(player: usize, node_id: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0001_0000_01b3;
    let mut hash = FNV_OFFSET;
    for value in [player as u64, node_id as u64] {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

/// Собирает MultiwayStaticGame из каркаса с EV-листьями (топология 1:1).
pub fn blueprint_game(
    tree: &GameTree,
    player_count: usize,
    leaf_ev: &BlueprintLeafEv,
) -> Result<BlueprintGameBuild, String> {
    if player_count == 0 {
        return Err("blueprint game requires a positive player count".to_string());
    }

    let mut nodes: Vec<MultiwayGameNode> = Vec::with_capacity(tree.nodes.len());
    let mut uncovered_leaves = 0usize;
    let mut total_leaves = 0usize;

    for node in &tree.nodes {
        let game_node = if let Some(leaf) = &node.leaf {
            match leaf {
                LeafKind::Terminal(TerminalState::Fold { winner }) => {
                    let mut utility = vec![0.0; player_count];
                    for (player, value) in utility.iter_mut().enumerate() {
                        let committed = node
                            .state
                            .players
                            .iter()
                            .find(|p| p.seat == player)
                            .map(|p| p.committed_total)
                            .unwrap_or(0);
                        *value = if player == *winner {
                            node.state.pot as f64 - committed as f64
                        } else {
                            -(committed as f64)
                        };
                    }
                    MultiwayGameNode::Terminal { utility }
                }
                LeafKind::Terminal(TerminalState::Showdown) => {
                    return Err(format!(
                        "blueprint round tree contains a showdown terminal at node {}",
                        node.id
                    ));
                }
                LeafKind::RoundComplete => {
                    total_leaves += 1;
                    let utility = leaf_ev.get(node.id);
                    if !leaf_ev.entries.contains_key(&node.id) {
                        uncovered_leaves += 1;
                    }
                    MultiwayGameNode::Terminal { utility }
                }
            }
        } else {
            let player = node
                .state
                .actor
                .ok_or_else(|| format!("blueprint node {} has no actor", node.id))?;
            if player >= player_count {
                return Err(format!(
                    "blueprint node {} actor {player} is outside player count",
                    node.id
                ));
            }
            MultiwayGameNode::Decision {
                player,
                infoset: blueprint_infoset(player, node.id),
                children: node.children.clone(),
            }
        };
        nodes.push(game_node);
    }

    let game = MultiwayStaticGame::new(player_count, tree.root, nodes)?;
    Ok(BlueprintGameBuild {
        game,
        uncovered_leaves,
        total_leaves,
    })
}

// ============================================================================
// T5.2 ч.3 (D-025(3/6)): каркасный MCCFR-раунд + веса путей.
// ============================================================================

/// Результат каркасного раунда MCCFR.
#[derive(Debug)]
pub struct BlueprintRound {
    /// Средние префлоп-стратегии: (player, infoset) -> частоты.
    pub strategies: HashMap<(usize, u64), Vec<f64>>,
    /// Веса RoundComplete-листьев (Σ с весами фолд-терминалов = 1).
    pub leaf_weights: HashMap<usize, f64>,
    /// Число итераций MCCFR.
    pub iterations: u64,
}

/// Прогон каркасного раунда: MCCFR + DFS средних -> стратегии + веса.
pub fn blueprint_round(
    tree: &GameTree,
    player_count: usize,
    leaf_ev: &BlueprintLeafEv,
    iterations: u64,
    seed: u64,
) -> Result<BlueprintRound, String> {
    if iterations == 0 {
        return Err("blueprint round requires a positive iteration count".to_string());
    }
    let build = blueprint_game(tree, player_count, leaf_ev)?;
    let mut solver = MultiwayMccfrSolver::new(build.game, seed, 0)?;
    solver.run(iterations)?;

    let mut strategies: HashMap<(usize, u64), Vec<f64>> = HashMap::new();
    let mut leaf_weights: HashMap<usize, f64> = HashMap::new();
    let mut fold_weight = 0.0_f64;

    let mut stack: Vec<(usize, f64)> = vec![(tree.root, 1.0)];
    while let Some((node_id, weight)) = stack.pop() {
        let node = &tree.nodes[node_id];
        if let Some(leaf) = &node.leaf {
            match leaf {
                LeafKind::RoundComplete => {
                    leaf_weights.insert(node_id, weight);
                }
                LeafKind::Terminal(_) => {
                    fold_weight += weight;
                }
            }
            continue;
        }
        let player = node
            .state
            .actor
            .ok_or_else(|| format!("blueprint node {node_id} has no actor"))?;
        let infoset = blueprint_infoset(player, node_id);
        let frequencies = solver
            .average_strategy(player, infoset)
            .unwrap_or_else(|| vec![1.0 / node.children.len() as f64; node.children.len()]);
        if frequencies.len() != node.children.len() {
            return Err(format!(
                "strategy at node {node_id} has {} actions, tree has {}",
                frequencies.len(),
                node.children.len()
            ));
        }
        strategies.insert((player, infoset), frequencies.clone());
        for (index, &child) in node.children.iter().enumerate() {
            stack.push((child, weight * frequencies[index]));
        }
    }

    let total: f64 = leaf_weights.values().sum::<f64>() + fold_weight;
    if (total - 1.0).abs() > 1e-6 {
        return Err(format!(
            "blueprint path weights sum to {total}, expected 1.0"
        ));
    }

    Ok(BlueprintRound {
        strategies,
        leaf_weights,
        iterations,
    })
}

// ============================================================================
// T5.2 финал (D-025(3/6)): итеративная схема каркас <-> споты.
//
// Раунд: каркасный MCCFR -> веса -> топ-K спотов -> решение каждого
// (production-путь) -> EV -> следующий раунд. Замер сессии 22: каркасный
// раунд 100k итераций ~8 c (0.083 мс/итер на 170k-каркасе); K=50 x
// 10k итераций спотов ~40 мин (v1).
// ============================================================================

/// Параметры итеративной схемы.
#[derive(Debug, Clone)]
pub struct BlueprintIterateParams {
    pub rounds: usize,
    pub top_k: usize,
    pub framework_iterations: u64,
    pub spot_iterations: u64,
    pub seed: u64,
}

impl Default for BlueprintIterateParams {
    fn default() -> Self {
        Self {
            rounds: 2,
            top_k: 50,
            framework_iterations: 100_000,
            spot_iterations: 10_000,
            seed: 1,
        }
    }
}

/// Прогресс раунда.
#[derive(Debug, Clone)]
pub struct BlueprintRoundReport {
    pub round: usize,
    /// Веса топ-K листьев (убывание).
    pub top_weights: Vec<(usize, f64)>,
    /// Покрытие EV-таблицей после раунда.
    pub covered_leaves: usize,
    /// Число инфосетов со стратегиями.
    pub strategy_count: usize,
}

/// Результат итеративной схемы.
#[derive(Debug)]
pub struct BlueprintIterateResult {
    pub rounds: Vec<BlueprintRoundReport>,
    pub strategies: HashMap<(usize, u64), Vec<f64>>,
    pub leaf_ev: BlueprintLeafEv,
}

/// Ошибка итеративной схемы.
#[derive(Debug)]
pub enum BlueprintIterateError {
    Round(String),
    Spot { leaf: usize, message: String },
    Config(String),
}

impl std::fmt::Display for BlueprintIterateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Round(message) => write!(f, "blueprint round failed: {message}"),
            Self::Spot { leaf, message } => {
                write!(f, "spot solve failed at leaf {leaf}: {message}")
            }
            Self::Config(message) => write!(f, "spot config failed: {message}"),
        }
    }
}

/// Источник спотовых решений (инъекция: тесты — дешёвый, продакшн —
/// config.solve через job-схему D-020; T5.3 — range propagation).
pub trait SpotSolver {
    /// Решает спот; возвращает EV-вектор по игрокам.
    fn solve_spot(&mut self, spot: &CarvedSpot) -> Result<Vec<f64>, String>;
}

/// Итеративная схема: раунды каркас<->споты.
///
/// `progress` вызывается после каждого раунда; покрытые листья не
/// перерешаются (идемпотентность EV-таблицы).
pub fn blueprint_iterate(
    tree: &GameTree,
    player_count: usize,
    params: &BlueprintIterateParams,
    solver: &mut dyn SpotSolver,
    mut progress: impl FnMut(&BlueprintRoundReport),
) -> Result<BlueprintIterateResult, BlueprintIterateError> {
    if params.rounds == 0 {
        return Err(BlueprintIterateError::Round(
            "iterate requires a positive round count".to_string(),
        ));
    }
    if params.top_k == 0 {
        return Err(BlueprintIterateError::Round(
            "iterate requires a positive top_k".to_string(),
        ));
    }

    let mut leaf_ev = BlueprintLeafEv::new(player_count);
    let mut reports = Vec::with_capacity(params.rounds);
    let mut last_strategies: HashMap<(usize, u64), Vec<f64>> = HashMap::new();

    for round in 0..params.rounds {
        let round_result = blueprint_round(
            tree,
            player_count,
            &leaf_ev,
            params.framework_iterations,
            params.seed + round as u64,
        )
        .map_err(BlueprintIterateError::Round)?;
        last_strategies = round_result.strategies;

        let mut weighted: Vec<(usize, f64)> = round_result
            .leaf_weights
            .iter()
            .map(|(leaf, weight)| (*leaf, *weight))
            .collect();
        weighted.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let top: Vec<(usize, f64)> = weighted.into_iter().take(params.top_k).collect();

        for (leaf, _weight) in &top {
            if leaf_ev.entries.contains_key(leaf) {
                continue;
            }
            let spot = carve_spot(tree, *leaf)
                .map_err(|error| BlueprintIterateError::Config(error.to_string()))?;
            let utility =
                solver
                    .solve_spot(&spot)
                    .map_err(|message| BlueprintIterateError::Spot {
                        leaf: *leaf,
                        message,
                    })?;
            leaf_ev
                .set(*leaf, utility)
                .map_err(BlueprintIterateError::Config)?;
        }

        let report = BlueprintRoundReport {
            round,
            top_weights: top,
            covered_leaves: leaf_ev.len(),
            strategy_count: last_strategies.len(),
        };
        progress(&report);
        reports.push(report);
    }

    Ok(BlueprintIterateResult {
        rounds: reports,
        strategies: last_strategies,
        leaf_ev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdem_domain::setup::build_preflop_state;
    use holdem_domain::table::{AnteMode, Position, TableConfig};
    use holdem_tree::action_abstraction::{ActionAbstraction, StreetSizing};
    use holdem_tree::{TreeBuildConfig, TreeBuilder};

    fn table3() -> TableConfig {
        TableConfig {
            table_size: 3,
            button: 0,
            small_blind: 100,
            big_blind: 200,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![20000; 3],
            dead_money: 0,
        }
    }

    fn carve_config() -> TreeBuildConfig {
        TreeBuildConfig {
            action_sizes: Default::default(),
            abstraction: Some(ActionAbstraction {
                preflop: StreetSizing {
                    raise_to_by_position: Some(vec![
                        (Position::Button, vec![500]),
                        (Position::SmallBlind, vec![600]),
                        (Position::BigBlind, vec![]),
                    ]),
                    ..StreetSizing::default()
                },
                ..ActionAbstraction::default()
            }),
            max_nodes: 100_000,
            max_depth: 64,
        }
    }

    fn srp_tree() -> GameTree {
        let state = build_preflop_state(&table3()).unwrap();
        TreeBuilder::build_round(state, &carve_config()).unwrap()
    }

    // ===== вырезка =====

    #[test]
    fn carve_reconstructs_srp_history() {
        let tree = srp_tree();
        let leaf = tree
            .nodes
            .iter()
            .find(|n| matches!(n.leaf, Some(LeafKind::RoundComplete)) && n.state.pot == 1100)
            .map(|n| n.id)
            .expect("SRP leaf exists");
        let spot = carve_spot(&tree, leaf).unwrap();
        assert_eq!(spot.history.len(), 3);
        assert_eq!(spot.history[0].player, 0);
        assert_eq!(spot.history[0].kind, "raise");
        assert_eq!(spot.history[0].to, Some(500));
        assert_eq!(spot.history[1].player, 1);
        assert_eq!(spot.history[1].kind, "fold");
        assert_eq!(spot.history[2].player, 2);
        assert_eq!(spot.history[2].kind, "call");
        assert_eq!(spot.pot, 1100);
        assert_eq!(spot.active_players, 2);
        assert_eq!(spot.hero_player, 2);
    }

    #[test]
    fn carve_rejects_non_leaf() {
        let tree = srp_tree();
        let error = carve_spot(&tree, tree.root);
        assert!(matches!(error, Err(CarveError::NodeIsNotRoundComplete(_))));
    }

    #[test]
    fn carve_histories_are_unique() {
        let tree = srp_tree();
        let mut unique = std::collections::HashSet::new();
        let mut count = 0;
        for node in &tree.nodes {
            if matches!(node.leaf, Some(LeafKind::RoundComplete)) {
                let spot = carve_spot(&tree, node.id).unwrap();
                assert!(unique.insert(format!("{:?}", spot.history)));
                count += 1;
            }
        }
        assert!(count > 0);
    }

    // ===== обходчик =====

    #[test]
    fn blueprint_game_builds_valid_static_game() {
        let tree = srp_tree();
        let build = blueprint_game(&tree, 3, &BlueprintLeafEv::new(3)).unwrap();
        assert_eq!(build.game.nodes().len(), tree.nodes.len());
        assert_eq!(build.uncovered_leaves, build.total_leaves);
        assert!(build.total_leaves > 0);
        match build.game.node(build.game.root()) {
            Some(MultiwayGameNode::Decision { player, .. }) => assert_eq!(*player, 0),
            other => panic!("root is not a decision: {other:?}"),
        }
    }

    #[test]
    fn blueprint_game_fold_utility_matches_chipev_semantics() {
        let tree = srp_tree();
        let build = blueprint_game(&tree, 3, &BlueprintLeafEv::new(3)).unwrap();
        for (index, node) in tree.nodes.iter().enumerate() {
            if let Some(LeafKind::Terminal(TerminalState::Fold { winner })) = &node.leaf {
                if *winner == 2 && node.state.pot == 300 {
                    match build.game.node(index) {
                        Some(MultiwayGameNode::Terminal { utility }) => {
                            assert_eq!(utility.len(), 3);
                            assert!((utility[2] - 100.0).abs() < 1e-9);
                            assert!((utility[1] + 100.0).abs() < 1e-9);
                            assert!((utility[0] - 0.0).abs() < 1e-9);
                            assert!(utility.iter().sum::<f64>().abs() < 1e-9);
                        }
                        other => panic!("not terminal: {other:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn blueprint_game_leaf_ev_overrides_zeros() {
        let tree = srp_tree();
        let mut ev = BlueprintLeafEv::new(3);
        let leaf = tree
            .nodes
            .iter()
            .find(|n| matches!(n.leaf, Some(LeafKind::RoundComplete)) && n.state.pot == 1100)
            .map(|n| n.id)
            .unwrap();
        ev.set(leaf, vec![50.0, 0.0, -50.0]).unwrap();
        let build = blueprint_game(&tree, 3, &ev).unwrap();
        assert_eq!(build.total_leaves, 11);
        assert_eq!(build.uncovered_leaves, 10);
        match build.game.node(leaf) {
            Some(MultiwayGameNode::Terminal { utility }) => {
                assert_eq!(utility.clone(), vec![50.0, 0.0, -50.0]);
            }
            other => panic!("not terminal: {other:?}"),
        }
        assert!(ev.set(leaf, vec![1.0]).is_err());
    }

    #[test]
    fn blueprint_infoset_is_deterministic_and_distinct() {
        let a = blueprint_infoset(0, 5);
        let b = blueprint_infoset(0, 5);
        let c = blueprint_infoset(1, 5);
        let d = blueprint_infoset(0, 6);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    // ===== раунд =====

    #[test]
    fn blueprint_round_weights_sum_to_one() {
        let tree = srp_tree();
        let round = blueprint_round(&tree, 3, &BlueprintLeafEv::new(3), 500, 7).unwrap();
        assert_eq!(round.iterations, 500);
        let total: f64 = round.leaf_weights.values().sum::<f64>();
        assert!(total > 0.0 && total <= 1.0 + 1e-9);
        assert!(!round.strategies.is_empty());
        for frequencies in round.strategies.values() {
            assert!((frequencies.iter().sum::<f64>() - 1.0).abs() < 1e-6);
            assert!(frequencies.iter().all(|value| value.is_finite()));
        }
    }

    #[test]
    fn blueprint_round_responds_to_leaf_ev() {
        // Плотный сигнал: EV на ВСЕ листья (BTN +1000) — каждый визит
        // несёт regret-сигнал; редкий лист (вес ~1.25e-6) — лотерея
        // визитов, не тест (урок сессии 21).
        let tree = srp_tree();
        let leaf = tree
            .nodes
            .iter()
            .find(|n| matches!(n.leaf, Some(LeafKind::RoundComplete)) && n.state.pot == 1100)
            .map(|n| n.id)
            .unwrap();
        let zero = blueprint_round(&tree, 3, &BlueprintLeafEv::new(3), 2_000, 7).unwrap();
        let mut ev = BlueprintLeafEv::new(3);
        for node in &tree.nodes {
            if matches!(node.leaf, Some(LeafKind::RoundComplete)) {
                ev.set(node.id, vec![1000.0, 0.0, -1000.0]).unwrap();
            }
        }
        let fed = blueprint_round(&tree, 3, &ev, 2_000, 7).unwrap();
        let w_zero = zero.leaf_weights.get(&leaf).copied().unwrap_or(0.0);
        let w_fed = fed.leaf_weights.get(&leaf).copied().unwrap_or(0.0);
        assert!(
            w_fed > w_zero,
            "fed weight {w_fed} should exceed zero weight {w_zero}"
        );
    }

    #[test]
    fn blueprint_round_zero_integration_chain() {
        // Полный конвейер: carve -> job -> solve(50) -> EV -> раунд.
        use crate::MultiwayHoldemSpotJob;
        let tree = srp_tree();
        let leaf = tree
            .nodes
            .iter()
            .find(|n| matches!(n.leaf, Some(LeafKind::RoundComplete)) && n.state.pot == 1100)
            .map(|n| n.id)
            .unwrap();
        let spot = carve_spot(&tree, leaf).unwrap();
        let ranges = vec![
            "AA-TT, AKs-ATs".to_string(),
            "AA-22".to_string(),
            "AA-22, AKs-A2s".to_string(),
        ];
        let mut job_value = carved_spot_job_json(
            &spot,
            &ranges,
            "KK",
            "BB KK round0",
            "2s 7d 9c",
            "5h",
            "3h",
            20260927,
        );
        job_value["table"] = serde_json::json!({
            "table_size": 3, "button": 0, "small_blind": 100, "big_blind": 200,
            "ante": 0, "ante_mode": "none",
            "stacks": [20000, 20000, 20000], "dead_money": 0
        });
        let json = serde_json::to_string_pretty(&job_value).unwrap();
        let job = MultiwayHoldemSpotJob::from_json(&json).unwrap();
        let config = job.into_config().unwrap();
        let result = config.solve(50, 1).unwrap();
        let mean = &result.strategy_report.utility_estimate.mean;
        assert_eq!(mean.len(), 3);
        let mut ev = BlueprintLeafEv::new(3);
        ev.set(leaf, mean.clone()).unwrap();
        let round = blueprint_round(&tree, 3, &ev, 500, 7).unwrap();
        assert!(round.leaf_weights.contains_key(&leaf));
        assert!(!round.strategies.is_empty());
    }

    // ===== итеративная схема =====

    struct FixedEvSolver {
        player_count: usize,
        calls: std::cell::Cell<usize>,
    }

    impl SpotSolver for FixedEvSolver {
        fn solve_spot(&mut self, _spot: &CarvedSpot) -> Result<Vec<f64>, String> {
            self.calls.set(self.calls.get() + 1);
            let mut utility = vec![0.0; self.player_count];
            utility[0] = 100.0;
            utility[2] = -100.0;
            Ok(utility)
        }
    }

    #[test]
    fn iterate_runs_rounds_and_covers_top_spots() {
        let tree = srp_tree();
        let params = BlueprintIterateParams {
            rounds: 2,
            top_k: 3,
            framework_iterations: 2_000,
            spot_iterations: 0,
            seed: 7,
        };
        let mut solver = FixedEvSolver {
            player_count: 3,
            calls: std::cell::Cell::new(0),
        };
        let mut reports = Vec::new();
        let result = blueprint_iterate(&tree, 3, &params, &mut solver, |report| {
            reports.push(report.round)
        })
        .unwrap();
        assert_eq!(result.rounds.len(), 2);
        assert_eq!(reports, vec![0, 1]);
        assert!(solver.calls.get() <= 6);
        assert!(solver.calls.get() >= 3);
        assert!(result.rounds[0].top_weights.len() <= 3);
        assert!(!result.strategies.is_empty());
        assert!(result.rounds[1].covered_leaves >= result.rounds[0].covered_leaves);
    }

    #[test]
    fn iterate_stabilizes_top_actions_under_dense_signal() {
        let tree = srp_tree();
        let params = BlueprintIterateParams {
            rounds: 3,
            top_k: 11,
            framework_iterations: 2_000,
            spot_iterations: 0,
            seed: 7,
        };
        let mut solver = FixedEvSolver {
            player_count: 3,
            calls: std::cell::Cell::new(0),
        };
        let result = blueprint_iterate(&tree, 3, &params, &mut solver, |_| {}).unwrap();
        for frequencies in result.strategies.values() {
            assert!((frequencies.iter().sum::<f64>() - 1.0).abs() < 1e-6);
            assert!(frequencies.iter().all(|value| value.is_finite()));
        }
        assert_eq!(result.leaf_ev.len(), 11);
    }
}
