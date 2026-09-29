//! T5.2/D-025: вырезка постфлоп-спотов из префлоп-каркаса (Round-дерева).
//!
//! Каркас даёт структуру префлоп-действий (D-024(10a)); каждый лист типа
//! RoundComplete — закрытый префлоп-раунд. Вырезка превращает лист в job
//! спота (D-020-класс): история реконструируется по parent-links,
//! hero = первый постфлоп-актор, диапазоны наследуются от каркаса
//! (v1 — без range propagation, это T5.3/D-024(8)), борд — фикс.
//!
//! Дублей нет по построению: узел каркаса входит ровно в один спот.
//! Контракт реконструкции: действие ребра живёт в ДОЧЕРНЕМ узле
//! (`action_from_parent: Option<Action>` — как читают все
//! стратегии-репорты), а не списком у родителя.

use holdem_domain::PlayerId;
use holdem_tree::{GameTree, LeafKind};

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

    #[test]
    fn carve_reconstructs_srp_history() {
        let tree = srp_tree();
        // SRP-лист: pot 1100, active [BTN, BB] — единственный по дампу T5.2-10.
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
        // Hero = первый Active после баттона: SB сфолдил -> BB.
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
}
