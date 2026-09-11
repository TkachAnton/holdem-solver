use holdem_cards::{cards_from_str, mask_from_cards};
use holdem_domain::setup::build_preflop_state;
use holdem_domain::table::{AnteMode, TableConfig};
use holdem_ranges::{Combo, WeightedCombo, WeightedRange};
use holdem_solver_core::{
    compile_multiway_holdem_tree, multiway_private_deals_from_ranges,
    resume_compiled_multiway_holdem_game, run_compiled_multiway_holdem_game,
};
use holdem_tree::action_abstraction::{ActionAbstraction, StreetSizing};
use holdem_tree::{ChanceConfig, ChanceOutcome, FullTreeBuildConfig, TreeBuildConfig, TreeBuilder};

fn combo(text: &str) -> Combo {
    let cards = cards_from_str(text).unwrap();
    Combo::new(cards[0], cards[1]).unwrap()
}

fn range(entries: &[&str]) -> WeightedRange {
    WeightedRange {
        combos: entries
            .iter()
            .map(|&cards| {
                let combo = combo(cards);
                WeightedCombo {
                    combo,
                    class_id: combo.class_id(),
                    weight: 1.0,
                }
            })
            .collect(),
    }
}

fn three_way_tree() -> (holdem_tree::GameTree, u64) {
    let table = TableConfig {
        table_size: 3,
        button: 0,
        small_blind: 50,
        big_blind: 100,
        ante: 0,
        ante_mode: AnteMode::None,
        stacks: vec![10_000, 10_000, 10_000],
        dead_money: 0,
    };
    let config = FullTreeBuildConfig {
        round: TreeBuildConfig {
            action_sizes: Default::default(),
            abstraction: Some(ActionAbstraction {
                preflop: StreetSizing {
                    explicit_raise_to: vec![300],
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
            max_depth: 96,
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
        },
        postflop_order: vec![1, 2, 0],
    };
    let tree = TreeBuilder::build_full(build_preflop_state(&table).unwrap(), &config).unwrap();
    (tree, config.fingerprint())
}

#[test]
fn realistic_three_way_holdem_compiles_and_runs_external_sampling() {
    let (tree, config_fingerprint) = three_way_tree();
    assert!(tree.nodes.len() > 100);
    assert!(tree.chance_nodes().count() > 0);

    let ranges = [
        range(&["As Ah", "Kc Kd"]),
        range(&["Jc Js", "Td Th"]),
        range(&["Qs Qd", "8c 8d"]),
    ];
    let deals = multiway_private_deals_from_ranges(
        &[&ranges[0], &ranges[1], &ranges[2]],
        mask_from_cards(&[]).unwrap(),
        64,
    )
    .unwrap();
    assert_eq!(deals.len(), 8);
    assert!((deals.iter().map(|deal| deal.probability).sum::<f64>() - 1.0).abs() < 1e-12);

    let compiled = compile_multiway_holdem_tree(&tree, &deals).unwrap();
    assert!(compiled.node_count() > tree.nodes.len());
    assert_eq!(compiled.private_deal_count(), 8);

    let first =
        run_compiled_multiway_holdem_game(&compiled, 4, 20260910, config_fingerprint).unwrap();
    assert_eq!(first.average_utility.len(), 3);
    assert!(first.average_utility.iter().all(|value| value.is_finite()));
    assert!((first.average_utility.iter().sum::<f64>()).abs() < 1e-8);
    assert_eq!(first.checkpoint.player_count, 3);
    assert!(first.checkpoint.metrics.sampled_chance_nodes > 0);
    assert!(first.checkpoint.metrics.sampled_opponent_actions > 0);

    let resumed =
        resume_compiled_multiway_holdem_game(&compiled, &first.checkpoint, 2, config_fingerprint)
            .unwrap();
    assert_eq!(resumed.checkpoint.iterations, 6);
    assert_eq!(resumed.average_utility.len(), 3);
    assert!(resumed
        .average_utility
        .iter()
        .all(|value| value.is_finite()));
}
