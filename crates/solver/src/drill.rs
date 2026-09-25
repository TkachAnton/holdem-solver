//! T4.3/D-022: дрилл v0 — движок учебного цикла (D-003).
//!
//! Рука: спот показан (борд, банк, позиция, рука героя), стратегия скрыта;
//! пользователь вводит действие; движок считает честную потерю EV —
//! paired conditioned-MC против средней стратегии (baseline = средняя
//! везде; forced = действие героя в корне + средняя ниже; дилы
//! кондиционированы на руку героя) — затем раскрывает частоты средней
//! стратегии для этой руки. Сессия сохраняется атомарно после каждой
//! руки (паттерн job-store), прогресс переживает прерывание.
//!
//! Мера несмещённая: V-таблица best-response поиска (D-021(7)) здесь не
//! используется — её смещение поиска задокументировано; она остаётся
//! субстратом оптимизации на будущее.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use holdem_cards::{card_str, Card};
use holdem_domain::{Action, Chips, PlayerId, Street};
use holdem_ranges::Combo;
use holdem_tree::GameTree;

use crate::multiway_batch::MultiwayHoldemBatchSolver;
use crate::multiway_spot::MultiwayHoldemSpotConfig;
use crate::{multiway_holdem_tree_fingerprint, XorShift64};

pub const DRILL_SESSION_SCHEMA_VERSION: u32 = 1;
pub const DRILL_DEFAULT_HANDS: usize = 10;

/// Параметры сессии дрилла (T4.3/D-022).
#[derive(Debug, Clone)]
pub struct DrillParams {
    /// Число рук сессии (DoD v0 — 10).
    pub hands: usize,
    /// Число conditioned-дилов на руку (paired: baseline и forced на одних дилах).
    pub ev_samples: usize,
    pub max_private_attempts: usize,
    /// Seed выбора рук (движок держит собственный XorShift64).
    pub seed: u64,
}

impl Default for DrillParams {
    fn default() -> Self {
        Self {
            hands: DRILL_DEFAULT_HANDS,
            ev_samples: 256,
            max_private_attempts: 100_000,
            seed: 0,
        }
    }
}

/// Paired-оценка потери EV одной руки (T4.3/D-022).
#[derive(Debug, Clone, PartialEq)]
pub struct DrillHandOutcome {
    pub deals: u64,
    pub loss_mean: f64,
    pub loss_variance: f64,
    pub loss_standard_error: f64,
}

/// Запрос ввода: всё, что видит пользователь до решения. Частоты средней
/// стратегии сюда не входят by design — раскрытие только после ввода.
#[derive(Debug, Clone)]
pub struct DrillPrompt {
    pub hand_index: usize,
    pub hands_total: usize,
    pub hero_player: PlayerId,
    pub hero_label: String,
    pub hero_hand_text: String,
    pub street: String,
    pub board_text: String,
    pub pot: Chips,
    pub big_blind: Chips,
    pub action_labels: Vec<String>,
}

pub type DrillResponder<'a> = dyn FnMut(&DrillPrompt) -> Option<usize> + 'a;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DrillFrequency {
    pub action: String,
    pub frequency: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DrillHandRecord {
    pub hand_index: usize,
    pub hero_cards: [u8; 2],
    pub action: String,
    pub action_index: usize,
    pub loss_chips: f64,
    pub loss_bb: f64,
    pub loss_pot_percent: f64,
    pub loss_standard_error: f64,
    pub deals: u64,
    pub frequencies: Vec<DrillFrequency>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DrillSession {
    pub schema_version: u32,
    pub format: String,
    pub tree_fingerprint: u64,
    pub hero_player: PlayerId,
    pub hero_label: String,
    pub hands_total: usize,
    pub hands_completed: usize,
    pub big_blind: Chips,
    pub pot: Chips,
    pub total_loss_chips: f64,
    pub average_loss_chips: f64,
    pub hands: Vec<DrillHandRecord>,
}

impl DrillSession {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }
}

fn action_label(action: &Action) -> String {
    match action {
        Action::Fold => "fold".to_string(),
        Action::Check => "check".to_string(),
        Action::Call => "call".to_string(),
        Action::Bet { to } => format!("bet {to}"),
        Action::Raise { to } => format!("raise to {to}"),
        Action::AllIn => "all-in".to_string(),
    }
}

fn street_name(street: Street) -> &'static str {
    match street {
        Street::Preflop => "preflop",
        Street::Flop => "flop",
        Street::Turn => "turn",
        Street::River => "river",
    }
}

fn cards_text(cards: &[Card]) -> String {
    cards
        .iter()
        .map(|card| card_str(*card))
        .collect::<Vec<_>>()
        .join(" ")
}

fn select_weighted_hand(
    candidates: &[(Combo, f64)],
    rng: &mut XorShift64,
) -> Result<Combo, String> {
    let total: f64 = candidates.iter().map(|(_, weight)| *weight).sum();
    if total <= 0.0 || !total.is_finite() {
        return Err("drill hero hands have invalid total weight".to_string());
    }
    let draw = rng.next_unit();
    let mut cumulative = 0.0;
    for (hand, weight) in candidates {
        cumulative += weight / total;
        if draw < cumulative {
            return Ok(*hand);
        }
    }
    Ok(candidates[candidates.len() - 1].0)
}

static DRILL_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temporary_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("drill-session");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = DRILL_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{name}.{timestamp}.{counter}.tmp"))
}

fn write_session_atomically(path: &Path, session: &DrillSession) -> Result<(), String> {
    let text = session.to_json()?;
    let temporary = unique_temporary_path(path);
    fs::write(&temporary, text).map_err(|error| format!("write temporary drill file: {error}"))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_first_error) if path.exists() => {
            fs::remove_file(path)
                .map_err(|error| format!("replace existing drill file: {error}"))?;
            fs::rename(&temporary, path)
                .map_err(|error| format!("commit drill file after replacement: {error}"))
        }
        Err(error) => Err(format!("commit drill file: {error}")),
    }
}

/// Запускает сессию дрилла (T4.3/D-022). `responder` вызывается на каждую
/// руку: `Some(index)` — выбранное действие корня; `None` — закончить
/// (сессия уже сохранена после каждой руки). Возвращает итоговую сессию.
pub fn run_drill(
    config: &MultiwayHoldemSpotConfig,
    tree: &GameTree,
    solver: &MultiwayHoldemBatchSolver,
    params: &DrillParams,
    responder: &mut DrillResponder,
    session_path: Option<&Path>,
) -> Result<DrillSession, String> {
    if params.hands == 0 {
        return Err("drill hands must be positive".to_string());
    }
    if params.ev_samples == 0 {
        return Err("drill ev_samples must be positive".to_string());
    }
    let state = config.state_after_history()?;
    let actions = solver.drill_root_actions()?;
    if actions.is_empty() {
        return Err("drill root has no actions".to_string());
    }
    let action_labels: Vec<String> = actions.iter().map(action_label).collect();

    let hero_range = config
        .ranges
        .get(config.hero_player)
        .ok_or_else(|| "drill hero range is missing".to_string())?;
    let mut legal_hands: Vec<(Combo, f64)> = Vec::new();
    for &hand in &config.hero_hands {
        if hand.conflicts(config.dead_cards) {
            continue;
        }
        let weight: f64 = hero_range
            .combos
            .iter()
            .filter(|entry| entry.combo == hand && entry.weight > 0.0)
            .map(|entry| entry.weight)
            .sum();
        if weight > 0.0 && weight.is_finite() {
            legal_hands.push((hand, weight));
        }
    }
    if legal_hands.is_empty() {
        return Err("drill hero has no legal weighted hands".to_string());
    }

    let mut session = DrillSession {
        schema_version: DRILL_SESSION_SCHEMA_VERSION,
        format: "drill_session".to_string(),
        tree_fingerprint: multiway_holdem_tree_fingerprint(tree),
        hero_player: config.hero_player,
        hero_label: config.hero_label.clone(),
        hands_total: params.hands,
        hands_completed: 0,
        big_blind: config.table.big_blind,
        pot: state.pot,
        total_loss_chips: 0.0,
        average_loss_chips: 0.0,
        hands: Vec::new(),
    };
    let big_blind = config.table.big_blind.max(1) as f64;
    let pot = state.pot.max(1) as f64;
    let street = street_name(state.street).to_string();
    let board_text = cards_text(&state.board);
    let mut rng = XorShift64::new(params.seed);

    for hand_index in 0..params.hands {
        let hand = select_weighted_hand(&legal_hands, &mut rng)?;
        let hand_text = format!("{} {}", card_str(hand.cards[0]), card_str(hand.cards[1]));
        let prompt = DrillPrompt {
            hand_index,
            hands_total: params.hands,
            hero_player: config.hero_player,
            hero_label: config.hero_label.clone(),
            hero_hand_text: hand_text,
            street: street.clone(),
            board_text: board_text.clone(),
            pot: state.pot,
            big_blind: config.table.big_blind,
            action_labels: action_labels.clone(),
        };
        let Some(action_index) = responder(&prompt) else {
            break;
        };
        if action_index >= actions.len() {
            return Err(format!(
                "drill responder returned action index {action_index}, expected 0..{}",
                actions.len()
            ));
        }
        let outcome = solver.drill_hand_outcome(
            config.hero_player,
            hand,
            action_index,
            params.ev_samples,
            params.max_private_attempts,
        )?;
        let frequencies: Vec<DrillFrequency> = solver
            .drill_root_frequencies(config.hero_player, hand)?
            .into_iter()
            .map(|(action, frequency)| DrillFrequency {
                action: action_label(&action),
                frequency,
            })
            .collect();
        let record = DrillHandRecord {
            hand_index,
            hero_cards: hand.cards,
            action: action_labels[action_index].clone(),
            action_index,
            loss_chips: outcome.loss_mean,
            loss_bb: outcome.loss_mean / big_blind,
            loss_pot_percent: outcome.loss_mean / pot * 100.0,
            loss_standard_error: outcome.loss_standard_error,
            deals: outcome.deals,
            frequencies,
        };
        session.total_loss_chips += record.loss_chips;
        session.hands.push(record);
        session.hands_completed = session.hands.len();
        session.average_loss_chips = if session.hands_completed > 0 {
            session.total_loss_chips / session.hands_completed as f64
        } else {
            0.0
        };
        if let Some(path) = session_path {
            write_session_atomically(path, &session)?;
        }
    }
    // T4.3/D-022: финальная запись гарантирует файл сессии даже при
    // выходе до первой руки — сессия с нулём рук валидна.
    if let Some(path) = session_path {
        write_session_atomically(path, &session)?;
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiway_spot::{MultiwayHoldemSpotAction, MultiwayHoldemSpotTreeConfig};
    use holdem_cards::cards_from_str;
    use holdem_domain::table::{AnteMode, TableConfig};
    use holdem_domain::ActionSizes;
    use holdem_ranges::WeightedCombo;
    use holdem_tree::{ChanceConfig, ChanceOutcome, FullTreeBuildConfig, TreeBuildConfig};

    fn combo(text: &str) -> Combo {
        let cards = cards_from_str(text).unwrap();
        Combo::new(cards[0], cards[1]).unwrap()
    }

    fn range(entries: &[&str]) -> holdem_ranges::WeightedRange {
        holdem_ranges::WeightedRange {
            combos: entries
                .iter()
                .map(|text| {
                    let hand = combo(text);
                    WeightedCombo {
                        combo: hand,
                        class_id: hand.class_id(),
                        weight: 1.0,
                    }
                })
                .collect(),
        }
    }

    fn drill_config() -> MultiwayHoldemSpotConfig {
        MultiwayHoldemSpotConfig {
            table: TableConfig {
                table_size: 3,
                button: 0,
                small_blind: 1,
                big_blind: 2,
                ante: 0,
                ante_mode: AnteMode::None,
                stacks: vec![100, 100, 100],
                dead_money: 0,
            },
            action_history: vec![
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
            ],
            ranges: vec![
                range(&["As Ah", "Qs Qh"]),
                range(&["Kc Kd"]),
                range(&["Qs Qh", "Qd Qc"]),
            ],
            dead_cards: 0,
            board_cards: cards_from_str("2s 3d 4c").unwrap(),
            tree: MultiwayHoldemSpotTreeConfig::Full(FullTreeBuildConfig {
                round: TreeBuildConfig {
                    action_sizes: ActionSizes {
                        bet_to: vec![3, 6],
                        raise_to: vec![8, 12],
                        include_all_in: false,
                    },
                    abstraction: None,
                    max_nodes: 100_000,
                    max_depth: 32,
                },
                chance: ChanceConfig {
                    flop: Some(vec![ChanceOutcome::new(
                        cards_from_str("2s 3d 4c").unwrap(),
                        1.0,
                    )]),
                    turn: Some(vec![ChanceOutcome::new(cards_from_str("5h").unwrap(), 1.0)]),
                    river: Some(vec![ChanceOutcome::new(cards_from_str("6s").unwrap(), 1.0)]),
                    enumerate_exact: false,
                    max_outcomes_per_node: 10,
                },
                postflop_order: Vec::new(),
            }),
            hero_player: 2,
            hero_hands: vec![combo("Qs Qh"), combo("Qd Qc")],
            hero_label: "QQ".to_string(),
            seed: 7,
            config_fingerprint: 0,
            max_private_attempts: 1000,
            worker_count: 1,
            reduction_batch_size: 1,
            exploitability_samples: 0,
            card_abstraction: None,
            blocking_samples: 0,
        }
    }

    #[test]
    fn drill_ten_hands_scripted_answers_full_session() {
        let config = drill_config();
        let (tree, mut solver) = config.build_solver().unwrap();
        solver.run_parallel(8, 1, 1).unwrap();
        let path = std::env::temp_dir().join(format!(
            "holdem_drill_test_full_{}.json",
            std::process::id()
        ));
        let params = DrillParams {
            hands: 10,
            ev_samples: 8,
            max_private_attempts: 1000,
            seed: 7,
        };
        let mut calls = 0usize;
        let session = run_drill(
            &config,
            &tree,
            &solver,
            &params,
            &mut |prompt: &DrillPrompt| {
                calls += 1;
                assert_eq!(prompt.street, "flop");
                assert_eq!(prompt.board_text, "2s 3d 4c");
                assert_eq!(prompt.pot, 11);
                assert!(prompt.hero_hand_text.starts_with('Q'));
                assert!(!prompt.action_labels.is_empty());
                Some(0)
            },
            Some(&path),
        )
        .unwrap();
        assert_eq!(calls, 10);
        assert_eq!(session.hands_completed, 10);
        assert_eq!(session.hands.len(), 10);
        assert!(session.hands.iter().all(|record| {
            record.loss_chips.is_finite()
                && record.deals == 8
                && !record.frequencies.is_empty()
                && (record
                    .frequencies
                    .iter()
                    .map(|entry| entry.frequency)
                    .sum::<f64>()
                    - 1.0)
                    .abs()
                    < 1e-6
        }));
        assert!(session.tree_fingerprint != 0);
        let persisted = DrillSession::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // JSON round-trip: структура — точно, числа — с допуском: дефолтный
        // парсер serde_json (без float_roundtrip) может смещать f64 на 1 ulp —
        // бит-в-бит равенство через границу сериализации некорректно по дизайну.
        assert_eq!(persisted.hands_completed, session.hands_completed);
        assert_eq!(persisted.hands.len(), session.hands.len());
        for (saved, live) in persisted.hands.iter().zip(session.hands.iter()) {
            assert_eq!(saved.hand_index, live.hand_index);
            assert_eq!(saved.hero_cards, live.hero_cards);
            assert_eq!(saved.action, live.action);
            assert_eq!(saved.action_index, live.action_index);
            assert_eq!(saved.deals, live.deals);
            assert!((saved.loss_chips - live.loss_chips).abs() < 1e-9);
            assert!((saved.loss_bb - live.loss_bb).abs() < 1e-9);
            assert!((saved.loss_pot_percent - live.loss_pot_percent).abs() < 1e-9);
            assert!((saved.loss_standard_error - live.loss_standard_error).abs() < 1e-9);
            for (saved_frequency, live_frequency) in
                saved.frequencies.iter().zip(live.frequencies.iter())
            {
                assert_eq!(saved_frequency.action, live_frequency.action);
                assert!((saved_frequency.frequency - live_frequency.frequency).abs() < 1e-9);
            }
        }
        assert!((persisted.total_loss_chips - session.total_loss_chips).abs() < 1e-9);
        assert!((persisted.average_loss_chips - session.average_loss_chips).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drill_early_quit_persists_partial_session() {
        let config = drill_config();
        let (tree, mut solver) = config.build_solver().unwrap();
        solver.run_parallel(8, 1, 1).unwrap();
        let path = std::env::temp_dir().join(format!(
            "holdem_drill_test_quit_{}.json",
            std::process::id()
        ));
        let params = DrillParams {
            hands: 10,
            ev_samples: 4,
            max_private_attempts: 1000,
            seed: 7,
        };
        let mut answered = 0usize;
        let session = run_drill(
            &config,
            &tree,
            &solver,
            &params,
            &mut |prompt: &DrillPrompt| {
                answered += 1;
                if answered <= 3 {
                    Some(prompt.action_labels.len() - 1)
                } else {
                    None
                }
            },
            Some(&path),
        )
        .unwrap();
        assert_eq!(answered, 4);
        assert_eq!(session.hands_completed, 3);
        let persisted = DrillSession::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.hands_completed, 3);
        assert_eq!(persisted.hands.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drill_is_deterministic_for_identical_solver_state() {
        let config = drill_config();
        let (tree_a, mut solver_a) = config.build_solver().unwrap();
        solver_a.run_parallel(8, 1, 1).unwrap();
        let (tree_b, mut solver_b) = config.build_solver().unwrap();
        solver_b.run_parallel(8, 1, 1).unwrap();
        let params = DrillParams {
            hands: 6,
            ev_samples: 4,
            max_private_attempts: 1000,
            seed: 11,
        };
        let session_a = run_drill(
            &config,
            &tree_a,
            &solver_a,
            &params,
            &mut |_: &DrillPrompt| Some(0),
            None,
        )
        .unwrap();
        let session_b = run_drill(
            &config,
            &tree_b,
            &solver_b,
            &params,
            &mut |_: &DrillPrompt| Some(0),
            None,
        )
        .unwrap();
        assert_eq!(session_a, session_b);
    }
}
