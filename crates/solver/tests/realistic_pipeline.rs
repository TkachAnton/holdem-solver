use holdem_cards::{cards_from_str, mask_from_cards};
use holdem_domain::setup::build_preflop_state;
use holdem_domain::table::{AnteMode, TableConfig};
use holdem_ranges::{Combo, WeightedCombo, WeightedRange};
use holdem_solver_core::{
    aggregate_strategy_report_by_deals, build_strategy_report, compile_private_holdem_tree,
    private_deals_from_ranges, resume_compiled_holdem_game, run_compiled_holdem_game, ActionReport,
    SolverAlgorithm,
};
use holdem_tree::action_abstraction::{ActionAbstraction, StreetSizing};
use holdem_tree::{ChanceConfig, ChanceOutcome, FullTreeBuildConfig, TreeBuildConfig, TreeBuilder};

fn combo(text: &str) -> Combo {
    let cards = cards_from_str(text).expect("valid combo cards");
    Combo::new(cards[0], cards[1]).expect("valid combo")
}

fn weighted_range(entries: &[(&str, f64)]) -> WeightedRange {
    WeightedRange {
        combos: entries
            .iter()
            .map(|&(cards, weight)| {
                let combo = combo(cards);
                WeightedCombo {
                    combo,
                    class_id: combo.class_id(),
                    weight,
                }
            })
            .collect(),
    }
}

fn realistic_tree_config() -> FullTreeBuildConfig {
    FullTreeBuildConfig {
        round: TreeBuildConfig {
            action_sizes: Default::default(),
            abstraction: Some(ActionAbstraction {
                preflop: StreetSizing {
                    explicit_raise_to: vec![300, 800],
                    ..StreetSizing::default()
                },
                flop: StreetSizing {
                    explicit_bet_to: vec![500],
                    explicit_raise_to: vec![1_500],
                    ..StreetSizing::default()
                },
                turn: StreetSizing {
                    explicit_bet_to: vec![1_000],
                    explicit_raise_to: vec![3_000],
                    ..StreetSizing::default()
                },
                river: StreetSizing {
                    explicit_bet_to: vec![2_000],
                    explicit_raise_to: vec![6_000],
                    ..StreetSizing::default()
                },
            }),
            max_nodes: 100_000,
            max_depth: 64,
        },
        chance: ChanceConfig {
            flop: Some(vec![ChanceOutcome::new(
                cards_from_str("2s 7d 9c").unwrap(),
                1.0,
            )]),
            turn: Some(vec![ChanceOutcome::new(cards_from_str("5h").unwrap(), 1.0)]),
            river: Some(vec![ChanceOutcome::new(cards_from_str("Qh").unwrap(), 1.0)]),
            enumerate_exact: false,
            max_outcomes_per_node: 4,
            dead_cards: 0,
        },
        postflop_order: vec![1, 0],
    }
}

fn realistic_tree() -> (holdem_tree::GameTree, FullTreeBuildConfig) {
    let table = TableConfig {
        table_size: 2,
        button: 0,
        small_blind: 50,
        big_blind: 100,
        ante: 0,
        ante_mode: AnteMode::None,
        stacks: vec![10_000, 10_000],
        dead_money: 0,
    };
    let config = realistic_tree_config();
    let tree = TreeBuilder::build_full(build_preflop_state(&table).unwrap(), &config).unwrap();
    (tree, config)
}

fn realistic_ranges() -> [WeightedRange; 2] {
    [
        weighted_range(&[("As Ah", 2.0), ("Ad Ac", 2.0), ("Kc Kd", 1.0)]),
        weighted_range(&[("Jc Js", 1.0), ("Jd Jh", 1.0), ("Ts Th", 2.0)]),
    ]
}

fn assert_action_frequencies_are_normalized(actions: &[ActionReport]) {
    let total: f64 = actions.iter().map(|action| action.frequency).sum();
    assert!(
        (total - 1.0).abs() < 1e-9,
        "action frequencies sum to {total}"
    );
    assert!(actions
        .iter()
        .all(|action| action.frequency.is_finite() && action.frequency >= 0.0));
}

#[test]
fn realistic_heads_up_pipeline_builds_solves_and_exports_results() {
    let (tree, config) = realistic_tree();
    assert!(tree.nodes.len() > 100, "tree was unexpectedly small");
    assert!(tree.chance_nodes().count() > 0);
    assert!(tree.leaf_nodes().count() > 0);

    let ranges = realistic_ranges();
    let dead_cards = mask_from_cards(&[]).unwrap();
    let deals = private_deals_from_ranges([&ranges[0], &ranges[1]], dead_cards).unwrap();
    assert_eq!(deals.len(), 9);
    assert!((deals.iter().map(|deal| deal.probability).sum::<f64>() - 1.0).abs() < 1e-12);

    let compiled = compile_private_holdem_tree(&tree, &deals).unwrap();
    assert!(compiled.node_count() > tree.nodes.len());
    assert_eq!(compiled.private_deal_count(), deals.len());

    let result = run_compiled_holdem_game(
        &compiled,
        SolverAlgorithm::CfrPlus,
        5,
        20260910,
        config.fingerprint(),
    )
    .unwrap();
    assert_eq!(result.private_deals, 9);
    assert_eq!(result.checkpoint.config_fingerprint, config.fingerprint());
    assert!(result.average_utility.iter().all(|value| value.is_finite()));
    assert!((result.average_utility[0] + result.average_utility[1]).abs() < 1e-8);

    let exact_report = build_strategy_report(&compiled, &result.checkpoint).unwrap();
    assert!(!exact_report.infosets.is_empty());
    assert!(exact_report
        .infosets
        .iter()
        .all(|infoset| !infoset.actions.is_empty()));
    for infoset in &exact_report.infosets {
        assert_action_frequencies_are_normalized(&infoset.actions);
    }

    let range_report = aggregate_strategy_report_by_deals(&exact_report, &deals).unwrap();
    assert!(!range_report.classes.is_empty());
    let matrix = range_report.to_matrix().unwrap();
    assert!(!matrix.matrices.is_empty());
    assert!(matrix
        .matrices
        .iter()
        .all(|matrix| matrix.cells.len() == 169));

    let json = range_report.to_json().unwrap();
    assert!(json.contains("range_strategy_report"));
    assert!(json.contains("\"matrices\""));
    assert!(json.contains("\"schema_version\""));
    let csv = range_report.to_csv().unwrap();
    assert!(csv.starts_with("schema_version,format,algorithm"));
    assert!(csv.lines().count() > 169);
}

#[test]
fn realistic_checkpoint_resume_matches_one_shot_cfr_plus() {
    let (tree, config) = realistic_tree();
    let ranges = realistic_ranges();
    let deals =
        private_deals_from_ranges([&ranges[0], &ranges[1]], mask_from_cards(&[]).unwrap()).unwrap();
    let compiled = compile_private_holdem_tree(&tree, &deals).unwrap();
    let fingerprint = config.fingerprint();

    let one_shot =
        run_compiled_holdem_game(&compiled, SolverAlgorithm::CfrPlus, 6, 7, fingerprint).unwrap();
    let first_part =
        run_compiled_holdem_game(&compiled, SolverAlgorithm::CfrPlus, 3, 7, fingerprint).unwrap();
    let resumed = resume_compiled_holdem_game(&compiled, &first_part.checkpoint, 3).unwrap();

    assert_eq!(
        resumed.checkpoint.iterations,
        one_shot.checkpoint.iterations
    );
    assert_eq!(
        resumed.checkpoint.game_fingerprint,
        one_shot.checkpoint.game_fingerprint
    );
    assert_eq!(resumed.checkpoint.config_fingerprint, fingerprint);
    for (resumed_value, one_shot_value) in resumed
        .average_utility
        .iter()
        .zip(one_shot.average_utility.iter())
    {
        assert!((resumed_value - one_shot_value).abs() < 1e-9);
    }
    assert_eq!(
        resumed.checkpoint.infosets.len(),
        one_shot.checkpoint.infosets.len()
    );
    for (resumed_entry, one_shot_entry) in resumed
        .checkpoint
        .infosets
        .iter()
        .zip(one_shot.checkpoint.infosets.iter())
    {
        assert_eq!(
            (resumed_entry.player, resumed_entry.infoset),
            (one_shot_entry.player, one_shot_entry.infoset)
        );
        assert_eq!(resumed_entry.visits, one_shot_entry.visits);
        for (resumed_value, one_shot_value) in resumed_entry
            .strategy_sum
            .iter()
            .zip(one_shot_entry.strategy_sum.iter())
        {
            assert!((resumed_value - one_shot_value).abs() < 1e-9);
        }
    }
}
