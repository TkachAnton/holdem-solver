//! Пост-строительная абстракция карт публичного дерева (T3.2, D-019).
//!
//! Механизм: `GameTree -> GameTree` одним нисходящим проходом. На каждом
//! chance-узле исходы группируются по бакету итогового борда улицы
//! (абстракции T3.1); представитель группы — исход с лексикографически
//! наименьшими картами (по убыванию значения); вероятность группы —
//! сумма вероятностей членов; поддеревья прочих членов отбрасываются.
//! Масса сохраняется без ренормализации: сумма вероятностей на узле
//! не меняется.
//!
//! Компиляторы и солверы не меняются: дерево остаётся валидным
//! `GameTree`, инфосеты сливаются физически через общий узел-представитель,
//! точные комбо сохраняются (D-006), чекпойнты защищены fingerprint'ом
//! трансформированного дерева.
//!
//! Цена абстракции — числом (D-019): структурная часть (исходы до/после
//! по улицам) — точно в `CardAbstractionReport`; блокировка представителей
//! дилами/диапазонами — MC-оценка `estimate_representative_blocking` по
//! трансформированному дереву и группировкам. Семантика оценки: доля
//! chance-массы исходов, отброшенных из-за блокировки представителя
//! (натуральные блокировки членов, действовавшие бы и без абстракции,
//! не считаются; ветви действий оцениваются при равновероятной стратегии —
//! реальную стратегию знает только солвер). Финальная мера качества —
//! best_response_probe (T4.2).
//!
//! Абстракции улиц строятся лениво: улица не платит за построение, если
//! её chance-узлы не требуют группировки (единственный исход). Equity-режим
//! дорог (минуты release) и предназначен для офлайн-построений.

use std::collections::{hash_map::Entry, HashMap};

use holdem_cards::{mask_from_cards, Card, DeckMask};
use holdem_domain::Street as DomainStreet;
use holdem_ranges::WeightedRange;
use holdem_solver_abstraction::{
    FlopAbstraction, FlopEquityAbstraction, Granularity, Street as AbstractionStreet,
    StreetAbstraction, StreetEquityAbstraction,
};
use holdem_tree::{ChanceNodeSpec, ChanceOutcome, GameTree, NodeId, TreeNode};

use crate::multiway_holdem::MultiwayPrivateDealSampler;

/// Вкус абстракции: структурные бакеты v1 (мгновенно) или точное
/// эквити-уточнение v2 (D-017/D-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAbstractionMode {
    Structural,
    Equity,
}

/// Спека абстракции карт. Дефолтного режима нет (D-019): вызывающий
/// обязан выбрать явно — «тихий дорогой сюрприз» исключён by design.
#[derive(Debug, Clone)]
pub struct CardAbstractionSpec {
    pub mode: CardAbstractionMode,
    pub granularity: Granularity,
    /// Только для Equity: число квантильных групп (D-017), >= 2.
    pub equity_groups: usize,
}

impl CardAbstractionSpec {
    pub fn structural(granularity: Granularity) -> Self {
        Self {
            mode: CardAbstractionMode::Structural,
            granularity,
            equity_groups: 4,
        }
    }

    pub fn equity(granularity: Granularity, equity_groups: usize) -> Self {
        Self {
            mode: CardAbstractionMode::Equity,
            granularity,
            equity_groups,
        }
    }
}

/// Счётчики одной улицы: цена абстракции числом.
#[derive(Debug, Clone)]
pub struct StreetChanceStats {
    pub street: DomainStreet,
    pub nodes: usize,
    pub outcomes_before: u64,
    pub outcomes_after: u64,
}

/// Отчёт трансформа: режим, отпечатки использованных абстракций
/// (None — улица не потребовала построения) и структурная цена.
#[derive(Debug, Clone)]
pub struct CardAbstractionReport {
    pub mode: CardAbstractionMode,
    pub granularity: Granularity,
    pub equity_groups: usize,
    pub flop_fingerprint: Option<u64>,
    pub turn_fingerprint: Option<u64>,
    pub river_fingerprint: Option<u64>,
    pub chance_nodes: usize,
    pub outcomes_before: u64,
    pub outcomes_after: u64,
    pub per_street: Vec<StreetChanceStats>,
}

enum FlopAbstractionKind {
    Structural(FlopAbstraction),
    Equity(FlopEquityAbstraction),
}

impl FlopAbstractionKind {
    fn bucket_of_cards(&self, cards: &[u8]) -> Option<usize> {
        match self {
            FlopAbstractionKind::Structural(abstraction) => abstraction.bucket_of_cards(cards),
            FlopAbstractionKind::Equity(abstraction) => abstraction.bucket_of_cards(cards),
        }
    }

    fn fingerprint(&self) -> u64 {
        match self {
            FlopAbstractionKind::Structural(abstraction) => abstraction.fingerprint(),
            FlopAbstractionKind::Equity(abstraction) => abstraction.fingerprint(),
        }
    }
}

enum StreetAbstractionKind {
    Structural(StreetAbstraction),
    Equity(StreetEquityAbstraction),
}

impl StreetAbstractionKind {
    fn bucket_of_cards(&self, cards: &[u8]) -> Option<usize> {
        match self {
            StreetAbstractionKind::Structural(abstraction) => abstraction.bucket_of_cards(cards),
            StreetAbstractionKind::Equity(abstraction) => abstraction.bucket_of_cards(cards),
        }
    }

    fn fingerprint(&self) -> u64 {
        match self {
            StreetAbstractionKind::Structural(abstraction) => abstraction.fingerprint(),
            StreetAbstractionKind::Equity(abstraction) => abstraction.fingerprint(),
        }
    }
}

/// Ленивое построение абстракций улиц.
struct AbstractionCache {
    spec: CardAbstractionSpec,
    flop: Option<FlopAbstractionKind>,
    turn: Option<StreetAbstractionKind>,
    river: Option<StreetAbstractionKind>,
}

impl AbstractionCache {
    fn new(spec: CardAbstractionSpec) -> Self {
        Self {
            spec,
            flop: None,
            turn: None,
            river: None,
        }
    }

    fn flop(&mut self) -> Result<&FlopAbstractionKind, String> {
        if self.flop.is_none() {
            let built = match self.spec.mode {
                CardAbstractionMode::Structural => {
                    FlopAbstractionKind::Structural(FlopAbstraction::new(self.spec.granularity))
                }
                CardAbstractionMode::Equity => FlopAbstractionKind::Equity(
                    FlopEquityAbstraction::new(self.spec.granularity, self.spec.equity_groups)?,
                ),
            };
            self.flop = Some(built);
        }
        Ok(self.flop.as_ref().expect("flop abstraction was just built"))
    }

    fn street(&mut self, street: AbstractionStreet) -> Result<&StreetAbstractionKind, String> {
        let needs_build = match street {
            AbstractionStreet::Turn => self.turn.is_none(),
            AbstractionStreet::River => self.river.is_none(),
        };
        if needs_build {
            let mode = self.spec.mode;
            let granularity = self.spec.granularity;
            let equity_groups = self.spec.equity_groups;
            let built = match mode {
                CardAbstractionMode::Structural => {
                    StreetAbstractionKind::Structural(StreetAbstraction::new(street, granularity))
                }
                CardAbstractionMode::Equity => StreetAbstractionKind::Equity(
                    StreetEquityAbstraction::new(street, granularity, equity_groups)?,
                ),
            };
            match street {
                AbstractionStreet::Turn => self.turn = Some(built),
                AbstractionStreet::River => self.river = Some(built),
            }
        }
        Ok(match street {
            AbstractionStreet::Turn => self.turn.as_ref(),
            AbstractionStreet::River => self.river.as_ref(),
        }
        .expect("street abstraction was just built"))
    }

    fn bucket_for(
        &mut self,
        next_street: DomainStreet,
        board_after: &[u8],
    ) -> Result<usize, String> {
        match next_street {
            DomainStreet::Flop => self
                .flop()?
                .bucket_of_cards(board_after)
                .ok_or_else(|| format!("flop bucket lookup failed for board {board_after:?}")),
            DomainStreet::Turn => self
                .street(AbstractionStreet::Turn)?
                .bucket_of_cards(board_after)
                .ok_or_else(|| format!("turn bucket lookup failed for board {board_after:?}")),
            DomainStreet::River => self
                .street(AbstractionStreet::River)?
                .bucket_of_cards(board_after)
                .ok_or_else(|| format!("river bucket lookup failed for board {board_after:?}")),
            DomainStreet::Preflop => {
                Err("chance node leading to preflop is not supported".to_string())
            }
        }
    }
}

#[derive(Debug, Default)]
struct StreetAccumulator {
    nodes: usize,
    outcomes_before: u64,
    outcomes_after: u64,
}

#[derive(Debug, Default)]
struct StatsAccumulator {
    flop: StreetAccumulator,
    turn: StreetAccumulator,
    river: StreetAccumulator,
}

impl StatsAccumulator {
    fn street_mut(&mut self, street: DomainStreet) -> Option<&mut StreetAccumulator> {
        match street {
            DomainStreet::Flop => Some(&mut self.flop),
            DomainStreet::Turn => Some(&mut self.turn),
            DomainStreet::River => Some(&mut self.river),
            DomainStreet::Preflop => None,
        }
    }

    fn chance_nodes(&self) -> usize {
        self.flop.nodes + self.turn.nodes + self.river.nodes
    }

    fn outcomes_before(&self) -> u64 {
        self.flop.outcomes_before + self.turn.outcomes_before + self.river.outcomes_before
    }

    fn outcomes_after(&self) -> u64 {
        self.flop.outcomes_after + self.turn.outcomes_after + self.river.outcomes_after
    }

    fn into_per_street(self) -> Vec<StreetChanceStats> {
        let StatsAccumulator { flop, turn, river } = self;
        [
            (DomainStreet::Flop, flop),
            (DomainStreet::Turn, turn),
            (DomainStreet::River, river),
        ]
        .into_iter()
        .filter(|(_, acc)| acc.nodes > 0)
        .map(|(street, acc)| StreetChanceStats {
            street,
            nodes: acc.nodes,
            outcomes_before: acc.outcomes_before,
            outcomes_after: acc.outcomes_after,
        })
        .collect()
    }
}

fn sorted_desc(cards: &[u8]) -> Vec<u8> {
    let mut sorted = cards.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted
}

/// Член группы: карты исхода и его исходная (досливная) вероятность.
/// Нужен MC-оценке блокировки: вероятность естественно доступного члена
/// заблокированной группы — и есть потеря абстракции.
#[derive(Debug, Clone)]
pub struct ChanceMember {
    pub cards: Vec<Card>,
    pub probability: f64,
}

/// Группировка одного chance-узла после трансформа: представитель
/// (исход с наименьшими картами, вероятность = сумма членов), бакет,
/// индекс представителя в исходном списке и все члены группы.
#[derive(Debug, Clone)]
pub struct ChanceGrouping {
    pub rep_index: usize,
    pub rep_outcome: ChanceOutcome,
    pub bucket: usize,
    pub members: Vec<ChanceMember>,
}

/// Группировки исходов по бакетам, индексированные по node id нового
/// (трансформированного) дерева. Заполняются только для chance-узлов,
/// где реальная группировка происходила (более одного исхода).
#[derive(Debug, Clone, Default)]
pub struct ChanceGroupings {
    pub by_node: HashMap<NodeId, Vec<ChanceGrouping>>,
}

/// Группировка исходов chance-узла по бакетам итогового борда.
/// Группы — в порядке первого появления в исходном списке; представитель
/// — исход с лексикографически наименьшими картами (по убыванию);
/// вероятность группы — сумма вероятностей членов.
fn group_outcomes(
    chance: &ChanceNodeSpec,
    board_prefix: &[u8],
    cache: &mut AbstractionCache,
) -> Result<Vec<ChanceGrouping>, String> {
    let mut groups: Vec<ChanceGrouping> = Vec::new();
    let mut group_of_bucket: HashMap<usize, usize> = HashMap::new();
    for (index, outcome) in chance.outcomes.iter().enumerate() {
        let mut board_after = board_prefix.to_vec();
        board_after.extend_from_slice(&outcome.cards);
        let bucket = cache.bucket_for(chance.next_street, &board_after)?;
        let member = ChanceMember {
            cards: outcome.cards.clone(),
            probability: outcome.probability,
        };
        match group_of_bucket.entry(bucket) {
            Entry::Vacant(slot) => {
                slot.insert(groups.len());
                groups.push(ChanceGrouping {
                    rep_index: index,
                    rep_outcome: outcome.clone(),
                    bucket,
                    members: vec![member],
                });
            }
            Entry::Occupied(slot) => {
                let group = &mut groups[*slot.get()];
                group.rep_outcome.probability += outcome.probability;
                if sorted_desc(&outcome.cards) < sorted_desc(&group.rep_outcome.cards) {
                    // Смена представителя: карты заменяем, накопленная
                    // вероятность группы сохраняется.
                    group.rep_outcome.cards = outcome.cards.clone();
                    group.rep_index = index;
                }
                group.members.push(member);
            }
        }
    }
    Ok(groups)
}

/// Нисходящая перестройка дерева. `groupings` — опциональный сборщик:
/// None — простой трансформ; Some — дополнительно записывает группировки
/// (нужны MC-оценке блокировки). Единственная реализация прохода —
/// дублей нет.
fn rebuild_node(
    source: &GameTree,
    node_id: NodeId,
    cache: &mut AbstractionCache,
    stats: &mut StatsAccumulator,
    mut groupings: Option<&mut ChanceGroupings>,
    new_nodes: &mut Vec<TreeNode>,
) -> Result<NodeId, String> {
    let node = source
        .node(node_id)
        .ok_or_else(|| format!("unknown source node {node_id}"))?;
    let new_id = new_nodes.len();

    let (new_node, child_sources): (TreeNode, Vec<NodeId>) = if let Some(chance) = &node.chance {
        if matches!(chance.next_street, DomainStreet::Preflop) {
            return Err(format!("chance node {node_id} leads to preflop"));
        }
        let acc = stats
            .street_mut(chance.next_street)
            .expect("preflop rejected above");
        acc.nodes += 1;
        acc.outcomes_before += chance.outcomes.len() as u64;
        let (outcomes, children): (Vec<ChanceOutcome>, Vec<NodeId>) = if chance.outcomes.len() == 1
        {
            // Один исход: группировка тривиальна — абстракция улицы не
            // строится (лень) и группировка не записывается: потери
            // блокировки на таком узле нет по определению.
            acc.outcomes_after += 1;
            (chance.outcomes.clone(), vec![node.children[0]])
        } else {
            let groups = group_outcomes(chance, &node.state.board, cache)?;
            acc.outcomes_after += groups.len() as u64;
            let mut outcomes = Vec::with_capacity(groups.len());
            let mut children = Vec::with_capacity(groups.len());
            for group in &groups {
                outcomes.push(group.rep_outcome.clone());
                children.push(node.children[group.rep_index]);
            }
            if let Some(target) = groupings.as_deref_mut() {
                target.by_node.insert(new_id, groups);
            }
            (outcomes, children)
        };
        (
            TreeNode {
                id: new_id,
                parent: None,
                action_from_parent: node.action_from_parent.clone(),
                state: node.state.clone(),
                children: Vec::new(),
                leaf: None,
                chance: Some(ChanceNodeSpec {
                    next_street: chance.next_street,
                    outcomes,
                }),
            },
            children,
        )
    } else {
        (
            TreeNode {
                id: new_id,
                parent: None,
                action_from_parent: node.action_from_parent.clone(),
                state: node.state.clone(),
                children: Vec::new(),
                leaf: node.leaf.clone(),
                chance: None,
            },
            node.children.clone(),
        )
    };

    new_nodes.push(new_node);
    for child_source in child_sources {
        let child = rebuild_node(
            source,
            child_source,
            cache,
            stats,
            groupings.as_deref_mut(),
            new_nodes,
        )?;
        new_nodes[child].parent = Some(new_id);
        new_nodes[new_id].children.push(child);
    }
    Ok(new_id)
}

/// Применяет абстракцию карт к публичному дереву (D-019). Простая
/// обёртка над `apply_card_abstraction_detailed`: группировки нужны
/// только MC-оценке блокировки.
pub fn apply_card_abstraction(
    tree: GameTree,
    spec: &CardAbstractionSpec,
) -> Result<(GameTree, CardAbstractionReport), String> {
    let (tree, report, _groupings) = apply_card_abstraction_detailed(tree, spec)?;
    Ok((tree, report))
}

/// Вариант трансформа, возвращающий также группировки исходов по бакетам
/// (индексируются по node id нового дерева).
pub fn apply_card_abstraction_detailed(
    tree: GameTree,
    spec: &CardAbstractionSpec,
) -> Result<(GameTree, CardAbstractionReport, ChanceGroupings), String> {
    tree.validate()?;
    if spec.mode == CardAbstractionMode::Equity && spec.equity_groups < 2 {
        return Err(format!(
            "equity_groups must be at least 2, got {}",
            spec.equity_groups
        ));
    }
    let mut cache = AbstractionCache::new(spec.clone());
    let mut stats = StatsAccumulator::default();
    let mut groupings = ChanceGroupings::default();
    let mut new_nodes: Vec<TreeNode> = Vec::with_capacity(tree.nodes.len());
    rebuild_node(
        &tree,
        tree.root,
        &mut cache,
        &mut stats,
        Some(&mut groupings),
        &mut new_nodes,
    )?;
    let output = GameTree {
        root: 0,
        nodes: new_nodes,
    };
    output.validate()?;
    let report = CardAbstractionReport {
        mode: spec.mode,
        granularity: spec.granularity,
        equity_groups: spec.equity_groups,
        flop_fingerprint: cache.flop.as_ref().map(|a| a.fingerprint()),
        turn_fingerprint: cache.turn.as_ref().map(|a| a.fingerprint()),
        river_fingerprint: cache.river.as_ref().map(|a| a.fingerprint()),
        chance_nodes: stats.chance_nodes(),
        outcomes_before: stats.outcomes_before(),
        outcomes_after: stats.outcomes_after(),
        per_street: stats.into_per_street(),
    };
    Ok((output, report, groupings))
}

/// Предел попыток на один сэмпл дила в MC-оценке блокировки.
const MAX_SAMPLE_ATTEMPTS: usize = 10_000;

/// MC-оценка потери chance-массы из-за блокировки представителей
/// дилами/диапазонами (D-019): для сэмпла дилов считает рекурсивно по
/// трансформированному дереву долю массы исходов, отброшенных по вине
/// блокировки представителя. Натуральные блокировки (исход заблокирован
/// целиком — играл бы и без абстракции) не считаются. Ветви действий
/// оцениваются при равновероятной стратегии — документированное
/// приближение; финальная мера — best_response_probe (T4.2).
/// Оценка детерминирована своим seed-потоком, независимым от солверного
/// (SAMPLER_SEED_MIX в multiway_batch).
pub fn estimate_representative_blocking(
    tree: &GameTree,
    groupings: &ChanceGroupings,
    ranges: &[&WeightedRange],
    dead_cards: DeckMask,
    seed: u64,
    samples: usize,
) -> Result<BlockingEstimate, String> {
    if samples == 0 {
        return Err("blocking estimate requires positive samples".to_string());
    }
    const ESTIMATE_SEED_MIX: u64 = 0xe621_0749_3d8c_7a11;
    let mut sampler =
        MultiwayPrivateDealSampler::new(ranges, dead_cards, seed ^ ESTIMATE_SEED_MIX)?;
    let mut lost_mass = 0.0f64;
    let mut deals = 0usize;
    for _ in 0..samples {
        let sample = match sampler.sample(MAX_SAMPLE_ATTEMPTS) {
            Ok(sample) => sample,
            Err(_) => continue,
        };
        deals += 1;
        let hands_mask: DeckMask = sample.hands.iter().fold(0, |mask, hand| mask | hand.mask());
        lost_mass += blocking_walk(tree, tree.root, groupings, hands_mask)?;
    }
    if deals == 0 {
        return Err("blocking estimate could not sample any legal deal".to_string());
    }
    Ok(BlockingEstimate {
        deals,
        lost_mass,
        lost_fraction: lost_mass / deals as f64,
    })
}

/// Рекурсивный подсчёт потерянной массы для одного дила (маска рук).
/// Возвращает долю массы узла, теряемую из-за блокировки представителей
/// в его поддереве.
fn blocking_walk(
    tree: &GameTree,
    node_id: NodeId,
    groupings: &ChanceGroupings,
    hands_mask: DeckMask,
) -> Result<f64, String> {
    let node = tree
        .node(node_id)
        .ok_or_else(|| format!("unknown node {node_id} in blocking walk"))?;
    if let Some(grouping_list) = groupings.by_node.get(&node_id) {
        // Сгруппированный chance-узел: дети параллельны группировкам.
        debug_assert_eq!(grouping_list.len(), node.children.len());
        let mut lost = 0.0;
        for (index, grouping) in grouping_list.iter().enumerate() {
            let rep_mask =
                mask_from_cards(&grouping.rep_outcome.cards).map_err(|error| error.to_string())?;
            if rep_mask & hands_mask != 0 {
                // Представитель заблокирован: масса естественно доступных
                // членов группы теряется из-за абстракции.
                for member in &grouping.members {
                    let member_mask =
                        mask_from_cards(&member.cards).map_err(|error| error.to_string())?;
                    if member_mask & hands_mask == 0 {
                        lost += member.probability;
                    }
                }
            } else {
                let sub_lost = blocking_walk(tree, node.children[index], groupings, hands_mask)?;
                lost += sub_lost * grouping.rep_outcome.probability;
            }
        }
        Ok(lost)
    } else if let Some(chance) = &node.chance {
        // Единственный исход без группировки: натуральная блокировка
        // останавливает продолжение дила — потери абстракции дальше
        // не начисляются.
        let mask = mask_from_cards(&chance.outcomes[0].cards).map_err(|error| error.to_string())?;
        if mask & hands_mask != 0 {
            return Ok(0.0);
        }
        blocking_walk(tree, node.children[0], groupings, hands_mask)
    } else if node.children.is_empty() {
        Ok(0.0)
    } else {
        // Узел решений: равновероятная стратегия (приближение).
        let mut lost = 0.0;
        for &child in &node.children {
            lost += blocking_walk(tree, child, groupings, hands_mask)?;
        }
        Ok(lost / node.children.len() as f64)
    }
}

/// Результат MC-оценки блокировки представителей.
#[derive(Debug, Clone)]
pub struct BlockingEstimate {
    /// Число успешных сэмплов дилов.
    pub deals: usize,
    /// Суммарная потерянная масса по сэмплам.
    pub lost_mass: f64,
    /// Средняя доля потерянной массы на дил: lost_mass / deals.
    pub lost_fraction: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiway_batch::multiway_holdem_tree_fingerprint;
    use holdem_cards::cards_from_str;
    use holdem_domain::setup::build_preflop_state;
    use holdem_domain::table::{AnteMode, TableConfig};
    use holdem_domain::ActionSizes;
    use holdem_ranges::parse_range;
    use holdem_tree::{ChanceConfig, FullTreeBuildConfig, TreeBuildConfig, TreeBuilder};

    fn heads_up_table() -> TableConfig {
        TableConfig {
            table_size: 2,
            button: 0,
            small_blind: 500,
            big_blind: 1_000,
            ante: 0,
            ante_mode: AnteMode::None,
            stacks: vec![10_000, 10_000],
            dead_money: 0,
        }
    }

    fn outcome(text: &str) -> ChanceOutcome {
        ChanceOutcome::new(cards_from_str(text).unwrap(), 1.0)
    }

    fn full_tree(flop: &[&str], turn: &[&str], river: &[&str]) -> GameTree {
        let state = build_preflop_state(&heads_up_table()).unwrap();
        let config = FullTreeBuildConfig {
            round: TreeBuildConfig {
                action_sizes: ActionSizes {
                    bet_to: Vec::new(),
                    raise_to: vec![2_500],
                    include_all_in: false,
                },
                abstraction: None,
                max_nodes: 10_000,
                max_depth: 32,
            },
            chance: ChanceConfig {
                flop: Some(flop.iter().map(|text| outcome(text)).collect()),
                turn: Some(turn.iter().map(|text| outcome(text)).collect()),
                river: Some(river.iter().map(|text| outcome(text)).collect()),
                enumerate_exact: false,
                max_outcomes_per_node: 10_000,
                dead_cards: 0,
            },
            postflop_order: vec![1, 0],
        };
        TreeBuilder::build_full(state, &config).unwrap()
    }

    fn chance_nodes_for(tree: &GameTree, street: DomainStreet) -> Vec<&TreeNode> {
        tree.chance_nodes()
            .filter(|node| {
                node.chance
                    .as_ref()
                    .map(|chance| {
                        std::mem::discriminant(&chance.next_street)
                            == std::mem::discriminant(&street)
                    })
                    .unwrap_or(false)
            })
            .collect()
    }

    fn collect_chance_outcomes(tree: &GameTree) -> Vec<Vec<(Vec<u8>, u64)>> {
        tree.chance_nodes()
            .map(|node| {
                node.chance
                    .as_ref()
                    .unwrap()
                    .outcomes
                    .iter()
                    .map(|outcome| (outcome.cards.clone(), outcome.probability.to_bits()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn flop_twins_merge_and_representative_is_deterministic() {
        // Списки исходов ChanceConfig глобальны на улицу: карта ривера
        // подставляется под КАЖДУЮ флоп-ветвь и не должна конфликтовать
        // ни с одним бордом (2c уже на борде ветви 9h 7d 2c — тогда
        // build_full падает на duplicate в next_board).
        let tree = full_tree(&["As Ks Qs", "Ah Kh Qh", "9h 7d 2c"], &["Jh"], &["3s"]);
        let (transformed, report) = apply_card_abstraction(
            tree.clone(),
            &CardAbstractionSpec::structural(Granularity::Coarse),
        )
        .unwrap();

        let flop_nodes = chance_nodes_for(&transformed, DomainStreet::Flop);
        let original_flop_nodes = chance_nodes_for(&tree, DomainStreet::Flop);
        assert!(!flop_nodes.is_empty());
        assert_eq!(flop_nodes.len(), original_flop_nodes.len());
        for node in &flop_nodes {
            let chance = node.chance.as_ref().unwrap();
            assert_eq!(chance.outcomes.len(), 2);
            // Монотонные близнецы (мастевой изоморфизм) в одном бакете;
            // представитель — пиковая версия (минимум карт); радуга —
            // отдельный бакет; порядок групп — первое появление.
            assert_eq!(
                chance.outcomes[0].cards,
                cards_from_str("As Ks Qs").unwrap()
            );
            assert!((chance.outcomes[0].probability - 2.0 / 3.0).abs() < 1e-12);
            assert_eq!(
                chance.outcomes[1].cards,
                cards_from_str("9h 7d 2c").unwrap()
            );
            assert!((chance.outcomes[1].probability - 1.0 / 3.0).abs() < 1e-12);
        }
        // Тёрн/ривер — по одному исходу на узел: абстракции не строились (лень).
        for node in chance_nodes_for(&transformed, DomainStreet::Turn) {
            assert_eq!(node.chance.as_ref().unwrap().outcomes.len(), 1);
        }
        for node in chance_nodes_for(&transformed, DomainStreet::River) {
            assert_eq!(node.chance.as_ref().unwrap().outcomes.len(), 1);
        }
        assert!(report.flop_fingerprint.is_some());
        assert!(report.turn_fingerprint.is_none());
        assert!(report.river_fingerprint.is_none());
        let flop_stats = report
            .per_street
            .iter()
            .find(|stats| matches!(stats.street, DomainStreet::Flop))
            .unwrap();
        assert_eq!(flop_stats.nodes, flop_nodes.len());
        assert_eq!(flop_stats.outcomes_before, 3 * flop_nodes.len() as u64);
        assert_eq!(flop_stats.outcomes_after, 2 * flop_nodes.len() as u64);
    }

    #[test]
    fn turn_outcomes_group_by_coarse_bucket() {
        let tree = full_tree(&["As Ks Qs"], &["Js", "Jh", "2c"], &["Td"]);
        let (transformed, report) = apply_card_abstraction(
            tree.clone(),
            &CardAbstractionSpec::structural(Granularity::Coarse),
        )
        .unwrap();

        let turn_nodes = chance_nodes_for(&transformed, DomainStreet::Turn);
        let original_turn_nodes = chance_nodes_for(&tree, DomainStreet::Turn);
        assert_eq!(turn_nodes.len(), original_turn_nodes.len());
        assert!(!turn_nodes.is_empty());
        for node in &turn_nodes {
            let chance = node.chance.as_ref().unwrap();
            // Coarse-вывод: AKQJ-монотон (Js) — свой бакет (форма мастей
            // (1,4)); AKQJh и AKQ2c — один бакет (семейство [1,1,1,1],
            // форма (2,3), top/mid coarse совпадают, span выключен);
            // представитель — 2c (меньше карта).
            assert_eq!(
                chance.outcomes.len(),
                2,
                "cards: {:?}",
                chance
                    .outcomes
                    .iter()
                    .map(|outcome| &outcome.cards)
                    .collect::<Vec<_>>()
            );
            assert_eq!(chance.outcomes[0].cards, cards_from_str("Js").unwrap());
            assert!((chance.outcomes[0].probability - 1.0 / 3.0).abs() < 1e-12);
            assert_eq!(chance.outcomes[1].cards, cards_from_str("2c").unwrap());
            assert!((chance.outcomes[1].probability - 2.0 / 3.0).abs() < 1e-12);
        }
        // Единственный флоп-исход: абстракция флопа не строилась.
        assert!(report.flop_fingerprint.is_none());
        assert!(report.turn_fingerprint.is_some());
        assert!(report.river_fingerprint.is_none());
    }

    #[test]
    fn transform_is_deterministic_and_idempotent() {
        let tree = full_tree(&["As Ks Qs"], &["Js", "Jh", "2c"], &["Td"]);
        let spec = CardAbstractionSpec::structural(Granularity::Coarse);
        let (first, _) = apply_card_abstraction(tree.clone(), &spec).unwrap();
        let (second, _) = apply_card_abstraction(tree.clone(), &spec).unwrap();
        assert_eq!(
            multiway_holdem_tree_fingerprint(&first),
            multiway_holdem_tree_fingerprint(&second)
        );
        assert_eq!(
            collect_chance_outcomes(&first),
            collect_chance_outcomes(&second)
        );

        let (again, report) = apply_card_abstraction(first.clone(), &spec).unwrap();
        assert_eq!(
            multiway_holdem_tree_fingerprint(&again),
            multiway_holdem_tree_fingerprint(&first)
        );
        // Идемпотентность: в каждой группе остался один исход.
        assert_eq!(report.outcomes_before, report.outcomes_after);
    }

    #[test]
    fn round_tree_without_chance_is_unchanged() {
        let state = build_preflop_state(&heads_up_table()).unwrap();
        let config = TreeBuildConfig {
            action_sizes: ActionSizes {
                bet_to: Vec::new(),
                raise_to: vec![2_500],
                include_all_in: false,
            },
            abstraction: None,
            max_nodes: 10_000,
            max_depth: 32,
        };
        let tree = TreeBuilder::build_round(state, &config).unwrap();
        let (transformed, report) = apply_card_abstraction(
            tree.clone(),
            &CardAbstractionSpec::structural(Granularity::Fine),
        )
        .unwrap();
        assert_eq!(
            multiway_holdem_tree_fingerprint(&transformed),
            multiway_holdem_tree_fingerprint(&tree)
        );
        assert_eq!(report.chance_nodes, 0);
        assert!(report.per_street.is_empty());
        assert_eq!(report.outcomes_before, 0);
        assert!(report.flop_fingerprint.is_none());
    }

    #[test]
    fn grouping_structure_records_members_and_masses() {
        let tree = full_tree(&["As Ks Qs", "Ah Kh Qh", "9h 7d 2c"], &["Jh"], &["3s"]);
        let (transformed, _report, groupings) = apply_card_abstraction_detailed(
            tree,
            &CardAbstractionSpec::structural(Granularity::Coarse),
        )
        .unwrap();
        let flop_node = chance_nodes_for(&transformed, DomainStreet::Flop)[0];
        let list = groupings.by_node.get(&flop_node.id).unwrap();
        assert_eq!(list.len(), 2);
        // Группа монотонных: представитель As Ks Qs, масса 2/3, два члена.
        assert_eq!(
            list[0].rep_outcome.cards,
            cards_from_str("As Ks Qs").unwrap()
        );
        assert!((list[0].rep_outcome.probability - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(list[0].members.len(), 2);
        assert!((list[0].members[1].probability - 1.0 / 3.0).abs() < 1e-12);
        // Радуга: один член, масса 1/3.
        assert_eq!(list[1].members.len(), 1);
        assert!((list[1].rep_outcome.probability - 1.0 / 3.0).abs() < 1e-12);
        // Тёрн с единственным исходом не попадает в группировки.
        let turn_node = chance_nodes_for(&transformed, DomainStreet::Turn)[0];
        assert!(!groupings.by_node.contains_key(&turn_node.id));
    }

    #[test]
    fn blocking_walk_counts_only_representative_loss() {
        let tree = full_tree(&["As Ks Qs", "Ah Kh Qh", "9h 7d 2c"], &["Jh"], &["3s"]);
        let (transformed, _report, groupings) = apply_card_abstraction_detailed(
            tree,
            &CardAbstractionSpec::structural(Granularity::Coarse),
        )
        .unwrap();
        let flop_node = chance_nodes_for(&transformed, DomainStreet::Flop)[0];
        let mask_of = |text: &str| mask_from_cards(&cards_from_str(text).unwrap()).unwrap();
        // As блокирует представителя монотонной группы: теряется доступный
        // член Ah Kh Qh (1/3); заблокированный член не в счёт.
        let lost = blocking_walk(&transformed, flop_node.id, &groupings, mask_of("As")).unwrap();
        assert!((lost - 1.0 / 3.0).abs() < 1e-9);
        // As и Ks: доступный член всё ещё Ah Kh Qh — та же потеря.
        let mask_as_ks = mask_of("As") | mask_of("Ks");
        let lost = blocking_walk(&transformed, flop_node.id, &groupings, mask_as_ks).unwrap();
        assert!((lost - 1.0 / 3.0).abs() < 1e-9);
        // As и Ah: оба члена естественно заблокированы — потерь абстракции нет.
        let mask_as_ah = mask_of("As") | mask_of("Ah");
        let lost = blocking_walk(&transformed, flop_node.id, &groupings, mask_as_ah).unwrap();
        assert!(lost.abs() < 1e-12);
        // Ah (не представитель): натуральная блокировка члена — не потеря.
        let lost = blocking_walk(&transformed, flop_node.id, &groupings, mask_of("Ah")).unwrap();
        assert!(lost.abs() < 1e-12);
        // Несвязанная карта: потерь нет.
        let lost = blocking_walk(&transformed, flop_node.id, &groupings, mask_of("2d")).unwrap();
        assert!(lost.abs() < 1e-12);
    }

    #[test]
    fn blocking_estimate_is_deterministic_and_bounded() {
        let tree = full_tree(&["As Ks Qs", "Ah Kh Qh", "9h 7d 2c"], &["Jh"], &["3s"]);
        let (transformed, _report, groupings) = apply_card_abstraction_detailed(
            tree,
            &CardAbstractionSpec::structural(Granularity::Coarse),
        )
        .unwrap();
        let ranges: Vec<WeightedRange> = ["AA", "KK", "QQ"]
            .iter()
            .map(|text| WeightedRange::from_classes(&parse_range(text).unwrap()))
            .collect();
        let refs: Vec<&WeightedRange> = ranges.iter().collect();
        let first =
            estimate_representative_blocking(&transformed, &groupings, &refs, 0, 0, 32).unwrap();
        let second =
            estimate_representative_blocking(&transformed, &groupings, &refs, 0, 0, 32).unwrap();
        assert_eq!(first.deals, second.deals);
        assert!((first.lost_fraction - second.lost_fraction).abs() < 1e-12);
        assert!(first.deals > 0);
        assert!((0.0..=1.0).contains(&first.lost_fraction));
        // Другой сид: другая оценка, но в тех же границах.
        let other =
            estimate_representative_blocking(&transformed, &groupings, &refs, 0, 7, 32).unwrap();
        assert!((0.0..=1.0).contains(&other.lost_fraction));
    }

    #[test]
    fn blocking_estimate_rejects_zero_samples() {
        assert!(estimate_representative_blocking(
            &full_tree(&["As Ks Qs"], &["Jh"], &["3s"]),
            &ChanceGroupings::default(),
            &[],
            0,
            0,
            0
        )
        .is_err());
    }

    #[test]
    #[ignore] // equity-проход тёрна ~минута release: --release -- --ignored --nocapture
    fn equity_mode_smoke_release() {
        let tree = full_tree(&["As Ks Qs"], &["Js", "Jh", "2c"], &["Td"]);
        let spec = CardAbstractionSpec::equity(Granularity::Fine, 4);
        let (transformed, report) = apply_card_abstraction(tree, &spec).unwrap();
        for node in chance_nodes_for(&transformed, DomainStreet::Turn) {
            let chance = node.chance.as_ref().unwrap();
            assert!((1..=3).contains(&chance.outcomes.len()));
        }
        assert!(report.turn_fingerprint.is_some());
        // Единственный флоп-исход: дорогая флоп-эквити не строилась.
        assert!(report.flop_fingerprint.is_none());
        println!(
            "equity: outcomes {} -> {}",
            report.outcomes_before, report.outcomes_after
        );
    }
}
